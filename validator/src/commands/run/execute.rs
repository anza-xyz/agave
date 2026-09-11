use {
    crate::{
        admin_rpc_service::{self, StakedNodesOverrides, load_staked_nodes_overrides},
        bootstrap,
        cli::{self},
        commands::{
            FromClapArgMatches,
            run::{args::RunArgs, config_file},
        },
        ledger_lockfile, lock_ledger,
    },
    agave_snapshots::{
        ArchiveFormat, SnapshotInterval, SnapshotVersion,
        paths::BANK_SNAPSHOTS_DIR,
        snapshot_config::{SnapshotConfig, SnapshotUsage},
    },
    agave_votor::vote_history_storage,
    agave_votor_transport::MAX_ENDPOINTS,
    arc_swap::ArcSwap,
    bytesize::ByteSize,
    clap::{ArgMatches, crate_name, value_t, value_t_or_exit, values_t, values_t_or_exit},
    crossbeam_channel::unbounded,
    log::*,
    rand::{rng, seq::SliceRandom},
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDbConfig},
        accounts_file::AccountsFileProvider,
        accounts_index::{
            AccountSecondaryIndexes, AccountsIndexConfig, DEFAULT_NUM_ENTRIES_OVERHEAD,
            DEFAULT_NUM_ENTRIES_TO_EVICT, IndexLimit, IndexLimitThreshold,
            MINIMAL_THRESHOLD_NUM_BYTES, ScanFilter,
        },
        partitioned_rewards::PartitionedEpochRewardsConfig,
        utils::{
            create_all_accounts_run_and_snapshot_dirs, create_and_canonicalize_directories,
            create_and_canonicalize_directory,
        },
    },
    solana_clap_utils::input_parsers::{
        keypair_of, keypairs_of, parse_cpu_ranges, pubkey_of, value_of, values_of,
    },
    solana_clock::{DEFAULT_SLOTS_PER_EPOCH, Slot},
    solana_core::{
        banking_stage::transaction_scheduler::scheduler_controller::SchedulerConfig,
        consensus::tower_storage,
        repair::repair_handler::RepairHandlerType,
        resource_limits,
        snapshot_packager_service::SnapshotPackagerService,
        system_monitor_service::SystemMonitorService,
        tpu::MAX_VOTES_PER_SECOND,
        validator::{
            BlockProductionMethod, BlockVerificationMethod, SchedulerPacing, Validator,
            ValidatorConfig, ValidatorLogConfig, ValidatorStartProgress, ValidatorTpuConfig,
            is_snapshot_config_valid,
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
    solana_runtime::{runtime_config::RuntimeConfig, snapshot_utils},
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{simple_qos::SimpleQosConfig, swqos::SwQosConfig},
        quic::{QuicStreamerConfig, SimpleQosQuicStreamerConfig, SwQosQuicStreamerConfig},
    },
    solana_turbine::broadcast_stage::BroadcastStageType,
    solana_validator_exit::Exit,
    std::{
        collections::HashSet,
        env,
        fs::{self, File},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
        str::{self, FromStr},
        sync::{Arc, RwLock, atomic::AtomicBool},
    },
};
#[cfg(target_os = "linux")]
use {
    agave_cpu_utils::cpu_affinity,
    agave_xdp::device::NetworkDevice,
    solana_core::{
        system_monitor_service::XdpNetworkConfigReport,
        validator::{XdpModules, XdpTransmitSetup},
    },
    std::collections::BTreeSet,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    Initialize,
    Run,
}

#[cfg(target_os = "linux")]
struct ResolvedXdp {
    policy: config_file::RuntimeXdpConfig,
    device: Arc<NetworkDevice>,
    src_ip: Ipv4Addr,
}

pub fn execute(
    matches: &ArgMatches,
    solana_version: &str,
    operation: Operation,
    config: super::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    // Print the built-in policy before RunArgs parsing, which loads identity
    // material and canonicalizes the ledger directory.
    if matches.is_present("print_default_config") {
        print!("{}", config_file::default_config_toml());
        return Ok(());
    }
    // Debugging panics is easier with a backtrace
    if env::var_os("RUST_BACKTRACE").is_none() {
        // Safety: env update is made before any spawned threads might access the environment
        unsafe { env::set_var("RUST_BACKTRACE", "1") }
    }

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
        tpu_sigverify_threads,
        tpu_transaction_forward_receive_threads,
        tpu_transaction_receive_threads,
        tpu_vote_transaction_receive_threads,
        tvu_receive_threads,
        tvu_retransmit_threads,
        tvu_sigverify_threads,
        tvu_bls_sigverify_threads,
    } = cli::thread_args::parse_num_threads_args(matches);

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

    let bind_addresses = BindIpAddrs::new(parsed_bind_addresses(matches)?)
        .map_err(|err| format!("invalid bind_addresses: {err}"))?;

    let entrypoint_addrs = run_args.entrypoints;
    for addr in &entrypoint_addrs {
        if !run_args.socket_addr_space.check(addr) {
            Err(format!("invalid entrypoint address: {addr}"))?;
        }
    }
    // XDP is not needed for init — it only initializes the ledger and exits.
    // Also, init drops all Linux capabilities in main() so XDP setup would fail.
    #[cfg(target_os = "linux")]
    let xdp_transmit_config: Option<ResolvedXdp> =
        build_xdp_config(matches, &operation, &bind_addresses)?;

    // The file and CLI layers mean the same thing on every platform, so they are
    // parsed and validated here too. Only resolution is Linux-only, so XDP
    // settings stay inactive rather than being rejected.
    #[cfg(not(target_os = "linux"))]
    validate_config_file_without_xdp(matches)?;

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
        use caps::{
            CapSet,
            Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON, CAP_SYS_NICE},
        };

        let super::Config { primordial_caps } = config;

        let mut required_caps = HashSet::new();
        let mut retained_caps = HashSet::new();
        let mut supported_caps = HashSet::from_iter([
            CAP_BPF,
            CAP_NET_ADMIN,
            CAP_NET_RAW,
            CAP_PERFMON,
            CAP_SYS_NICE,
        ]);

        // make sure we keep any primordial caps
        supported_caps.extend(primordial_caps.clone());
        required_caps.extend(primordial_caps.clone());
        retained_caps.extend(primordial_caps.clone());

        if let Some(resolved) = xdp_transmit_config.as_ref() {
            required_caps.insert(CAP_NET_ADMIN);
            required_caps.insert(CAP_NET_RAW);
            if resolved.policy.zero_copy {
                required_caps.insert(CAP_BPF);
                required_caps.insert(CAP_PERFMON);
            }
        }

        let snapshot_packager_niceness_adj =
            value_t_or_exit!(matches, "snapshot_packager_niceness_adj", i8);

        if snapshot_packager_niceness_adj != 0 || run_args.json_rpc_config.rpc_niceness_adj != 0 {
            required_caps.insert(CAP_SYS_NICE);
            retained_caps.insert(CAP_SYS_NICE);
        }

        // lazy dev check
        assert!(
            required_caps.is_subset(&supported_caps),
            "required_caps contains a cap not in supported_caps",
        );

        // validate and minimize the permitted set
        let current_permitted =
            caps::read(None, CapSet::Permitted).expect("permitted capset to be readable");
        let missing_caps = required_caps
            .difference(&current_permitted)
            .collect::<Vec<_>>();
        if !missing_caps.is_empty() {
            error!(
                "the current configuration requires the following capabilities, which have not \
                 been permitted to the current process: {missing_caps:?}",
            );
            std::process::exit(1);
        }
        // warn about extraneous caps that no configuration requires
        let extra_caps = current_permitted
            .difference(&supported_caps)
            .collect::<Vec<_>>();
        if !extra_caps.is_empty() {
            warn!(
                "dropping extraneous capabilities ({extra_caps:?}) from the current process. \
                 consider removing them from your operational configuration.",
            );
        }

        // drop all caps that the current configuration does not require
        caps::set(None, CapSet::Effective, &required_caps)
            .expect("linux allows effective capset to be set");
        caps::set(None, CapSet::Permitted, &required_caps)
            .expect("linux allows permitted capset to be set");

        // XDP _MUST_ be setup _BEFORE_ the app spawns any threads to ensure linux
        // capabilities do not leak, leaving the process in a state where it could
        // potentially be used as a privilege escalation gadget
        let setup = match xdp_transmit_config {
            Some(resolved) => build_xdp_transmit_setup(resolved, exit.clone())
                .map(|(setup, report)| (Some(setup), Some(report))),
            None => Ok((None, None)),
        };

        // we're done with caps needed to init xdp now. remove them from our process
        caps::set(None, CapSet::Effective, &retained_caps)
            .expect("linux allows effective capset to be set");
        caps::set(None, CapSet::Permitted, &retained_caps)
            .expect("linux allows permitted capset to be set");

        // Only now that the extra capabilities are gone is it safe to leave: a bad
        // interface or CPU must not exit the process while it is still elevated.
        setup?
    };

    #[cfg(not(target_os = "linux"))]
    let (xdp_transmit_setup, xdp_network_config_report) = (None, None);

    #[cfg(target_os = "linux")]
    let poh_pinned_cpu_core = value_of(matches, "poh_pinned_cpu_core")
        .or_else(|| value_of(matches, "experimental_poh_pinned_cpu_core"))
        .or(poh_service::DEFAULT_PINNED_CPU_CORE);

    #[cfg(not(target_os = "linux"))]
    let poh_pinned_cpu_core = None;

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

    let account_indexes = AccountSecondaryIndexes::from_clap_arg_match(matches)?;

    let restricted_repair_only_mode = matches.is_present("restricted_repair_only_mode");
    let accounts_shrink_optimize_total_space =
        value_t_or_exit!(matches, "accounts_shrink_optimize_total_space", bool);
    let vote_use_quic = value_t_or_exit!(matches, "vote_use_quic", bool);

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

    let vote_history_storage: Arc<dyn vote_history_storage::VoteHistoryStorage> = Arc::new(
        vote_history_storage::FileVoteHistoryStorage::new(ledger_path.clone()),
    );

    let accounts_index_limit =
        value_t!(matches, "accounts_index_limit", String).unwrap_or_else(|err| err.exit());
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
        num_flush_threads: Some(accounts_index_flush_threads),
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
    // accounts-db-write-cache-limit-mb was deprecated in v4.2.0
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

    let accounts_db_config = AccountsDbConfig {
        index: Some(accounts_index_config),
        account_indexes: Some(account_indexes.clone()),
        bank_hash_details_dir: ledger_path.clone(),
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
        num_background_threads: Some(accounts_db_background_threads),
        num_foreground_threads: Some(accounts_db_foreground_threads),
        accounts_file_provider: AccountsFileProvider::AppendVec,
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

    let skip_transaction_signatures_in_status_cache =
        !run_args.json_rpc_config.full_api && !snapshot_config.should_generate_snapshots();
    if skip_transaction_signatures_in_status_cache {
        info!(
            "Transaction signatures will not be stored in the status cache because full RPC and \
             snapshot generation are disabled"
        );
    }

    let mut validator_config = ValidatorConfig {
        log_config,
        require_tower: matches.is_present("require_tower"),
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
        account_paths: account_run_paths,
        account_snapshot_paths,
        accounts_db_config,
        accounts_db_skip_shrink: true,
        accounts_db_force_initial_clean: matches.is_present("no_skip_initial_accounts_db_clean"),
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
        banking_trace_dir_byte_limit: value_t_or_exit!(
            matches,
            "banking_trace_dir_byte_limit",
            u64
        ),
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
            vote_history_storage: validator_config.vote_history_storage.clone(),
            staked_nodes_overrides,
            rpc_to_plugin_manager_sender,
        },
    );

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

    snapshot_utils::remove_tmp_snapshot_archives(
        &validator_config.snapshot_config.full_snapshot_archives_dir,
    );
    snapshot_utils::remove_tmp_snapshot_archives(
        &validator_config
            .snapshot_config
            .incremental_snapshot_archives_dir,
    );

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

    let tpu_quic_server_config = SwQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: tpu_transaction_receive_threads,
            stream_receive_window_size: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            max_stream_data_bytes: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
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
            stream_receive_window_size: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            max_stream_data_bytes: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
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
        ValidatorTpuConfig {
            vote_use_quic,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
            sigverify_threads: tpu_sigverify_threads,
        },
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
        use_registered_io_uring_buffers: resource_limits::check_memlock_limit_for_disk_io(
            solana_accounts_db::accounts_db::TOTAL_IO_URING_BUFFERS_SIZE_LIMIT,
        ),
        use_direct_io: !matches.is_present("no_accounts_db_snapshots_direct_io"),
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

fn parsed_bind_addresses(matches: &ArgMatches) -> Result<Vec<IpAddr>, String> {
    matches
        .values_of("bind_address")
        .expect("bind_address has a clap default")
        .map(|value| {
            solana_net_utils::parse_host(value)
                .map_err(|error| format!("invalid --bind-address `{value}`: {error}"))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn bind_address_conflict(count: usize, address: IpAddr) -> Option<&'static str> {
    if count > 1 {
        Some("XDP does not support multiple --bind-address values; select one IPv4 address")
    } else if address.is_ipv6() {
        Some("XDP transmit supports IPv4 only; supply an IPv4 --bind-address")
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn resolve_xdp_configuration(
    application: &config_file::CliApplication,
    bind_address_count: usize,
    bind_address: IpAddr,
    poh_core: Option<usize>,
) -> Result<(Option<ResolvedXdp>, Vec<String>), String> {
    if !application.config.xdp_active() {
        return Ok((None, config_file::validate_policy(&application.config)?));
    }
    if let Some(conflict) = bind_address_conflict(bind_address_count, bind_address) {
        return Err(format!("{conflict}, or pass --no-xdp"));
    }
    let allowed_cpus: BTreeSet<_> = cpu_affinity(None)
        .map_err(|error| format!("failed to query process CPU affinity for XDP: {error}"))?
        .into_iter()
        .map(|cpu| *cpu)
        .collect();
    let (policy, warnings) =
        config_file::resolve_runtime(&application.config, &allowed_cpus, poh_core)?;
    let device = resolve_xdp_device(&policy.interface_label, &policy.device)?;
    let src_ip = resolve_xdp_source_ipv4(&policy.interface_label, &device, bind_address)?;
    Ok((
        Some(ResolvedXdp {
            policy,
            device,
            src_ip,
        }),
        warnings,
    ))
}

#[cfg(target_os = "linux")]
fn build_xdp_transmit_setup(
    resolved: ResolvedXdp,
    exit: Arc<AtomicBool>,
) -> Result<(XdpTransmitSetup, XdpNetworkConfigReport), String> {
    use agave_xdp::transmitter::{TransmitterBuilder, XdpConfig};

    let ResolvedXdp {
        policy,
        device,
        src_ip,
    } = resolved;
    let config_file::RuntimeXdpConfig {
        interface_label: logical_interface,
        device: _,
        queues,
        zero_copy,
        modules,
    } = policy;
    let modules = XdpModules {
        tpu: modules.tpu,
        turbine: modules.turbine,
        repair: modules.repair,
        gossip: modules.gossip,
    };
    let xdp_interface = device.name().to_string();
    let transmitter_builder = TransmitterBuilder::new(
        XdpConfig::new(Some(xdp_interface.clone()), queues, zero_copy),
        exit,
    )
    .map_err(|e| {
        let remediation = if zero_copy {
            "Check the configured workers; if zero-copy is unsupported, pass --no-xdp-zero-copy, \
             or pass --no-xdp."
        } else {
            "Check the configured workers or pass --no-xdp."
        };
        format!(
            "failed to create the XDP transmitter for logical interface `{logical_interface}`, \
             device `{xdp_interface}`: {e}. {remediation}"
        )
    })?;
    Ok((
        XdpTransmitSetup {
            transmitter_builder,
            src_ip,
            modules,
        },
        XdpNetworkConfigReport {
            zero_copy,
            interface: xdp_interface,
        },
    ))
}

#[cfg(target_os = "linux")]
fn resolve_xdp_device(
    logical_interface: &str,
    selector: &config_file::DeviceSelector,
) -> Result<Arc<NetworkDevice>, String> {
    let device = match selector {
        config_file::DeviceSelector::Name(name) => NetworkDevice::new(name).map_err(|error| {
            format!(
                "XDP logical interface `{logical_interface}` selects device.name {name:?}, which \
                 is not usable: {error}; fix the name or pass --no-xdp"
            )
        })?,
        config_file::DeviceSelector::DefaultRoute => NetworkDevice::new_from_default_route()
            .map_err(|error| {
                format!(
                    "failed to open the default-route device for XDP logical interface \
                     `{logical_interface}`: {error}; set device.name or pass --no-xdp"
                )
            })?,
    };
    Ok(Arc::new(device))
}

#[cfg(target_os = "linux")]
fn resolve_xdp_source_ipv4(
    logical_interface: &str,
    device: &NetworkDevice,
    bind_ip: IpAddr,
) -> Result<Ipv4Addr, String> {
    match bind_ip {
        IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        IpAddr::V4(_) => agave_xdp::interface_ipv4(device.name()).map_err(|error| {
            format!(
                "cannot select an IPv4 source address for XDP logical interface \
                 `{logical_interface}`, device `{}`: {error}; assign an IPv4 address to the \
                 device, pass --bind-address, or pass --no-xdp",
                device.name()
            )
        }),
        IpAddr::V6(_) => Err(
            "XDP transmit supports IPv4 only; supply an IPv4 --bind-address or pass --no-xdp"
                .to_string(),
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn validate_config_file_without_xdp(matches: &ArgMatches) -> Result<(), String> {
    let user_path = matches.value_of("config_file");
    let effective = config_file::load(user_path.map(Path::new))?;
    let application = config_file::apply_cli(effective, cli_xdp_overrides(matches)?)?;
    for warning in &application.warnings {
        warn!("{warning}");
    }
    for warning in config_file::validate_policy(&application.config)? {
        warn!("{warning}");
    }
    // Only report inactivity the operator can act on. The built-in policy enables
    // XDP everywhere, so warning about it unprompted would fire on every startup.
    if user_path.is_some() && application.config.xdp_active() {
        warn!(
            "XDP transmit is unavailable on this platform; the configured XDP policy is valid but \
             inactive"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn build_xdp_config(
    matches: &ArgMatches,
    operation: &Operation,
    bind_addresses: &BindIpAddrs,
) -> Result<Option<ResolvedXdp>, String> {
    let effective = config_file::load(matches.value_of("config_file").map(Path::new))?;
    let overrides = cli_xdp_overrides(matches)?;
    if *operation == Operation::Initialize {
        info!("ledger initialization does not start XDP; skipping XDP policy validation");
        return Ok(None);
    }
    let application = config_file::apply_cli(effective, overrides)?;
    for warning in &application.warnings {
        warn!("{warning}");
    }
    let poh_pinned_cpu_core = value_of(matches, "poh_pinned_cpu_core")
        .or_else(|| value_of(matches, "experimental_poh_pinned_cpu_core"))
        .or(poh_service::DEFAULT_PINNED_CPU_CORE);
    let (resolved, warnings) = resolve_xdp_configuration(
        &application,
        bind_addresses.len(),
        bind_addresses.active(),
        poh_pinned_cpu_core,
    )?;
    for warning in warnings {
        warn!("{warning}");
    }
    if let Some(runtime) = &resolved {
        info!(
            "XDP policy: label={}, selector={:?}, device={}, source_ipv4={}, zero_copy={}, \
             workers={:?}, module sender positions: tpu={:?}, turbine={:?}, repair={:?}, \
             gossip={:?}",
            runtime.policy.interface_label,
            runtime.policy.device,
            runtime.device.name(),
            runtime.src_ip,
            runtime.policy.zero_copy,
            runtime.policy.queues,
            runtime.policy.modules.tpu,
            runtime.policy.modules.turbine,
            runtime.policy.modules.repair,
            runtime.policy.modules.gossip,
        );
    }
    Ok(resolved)
}

fn cli_xdp_overrides(matches: &ArgMatches) -> Result<config_file::CliOverrides, String> {
    let zero_copy = if matches.is_present("xdp_zero_copy") {
        Some(true)
    } else if matches.is_present("no_xdp_zero_copy") {
        Some(false)
    } else {
        None
    };
    Ok(config_file::CliOverrides {
        no_xdp: matches.is_present("no_xdp"),
        interface: matches.value_of("xdp_interface").map(str::to_string),
        cpu_cores: matches
            .value_of("xdp_cpu_cores")
            .map(|value| {
                parse_cpu_ranges(value)
                    .map_err(|error| format!("invalid --xdp-cpu-cores `{value}`: {error}"))
            })
            .transpose()?,
        zero_copy,
    })
}

#[cfg(all(target_os = "linux", test))]
mod versioned_xdp_tests {
    use {
        super::*,
        crate::{cli::DefaultArgs, commands::run::args::add_args},
        solana_net_utils::multihomed_sockets::BindIpAddrs,
        std::{io::Write as _, net::Ipv6Addr},
    };

    fn build_with_config(
        contents: &[u8],
        operation: Operation,
    ) -> Result<Option<ResolvedXdp>, String> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        let matches = app.get_matches_from(vec![
            "agave-validator",
            "--config-file",
            file.path().to_str().unwrap(),
        ]);
        let binds = BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()]).unwrap();
        build_xdp_config(&matches, &operation, &binds)
    }

    #[test]
    fn no_xdp_skips_host_resolution() {
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        let matches = app.get_matches_from(vec!["agave-validator", "--no-xdp"]);
        let binds = BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()]).unwrap();
        assert!(
            build_xdp_config(&matches, &Operation::Run, &binds)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn init_parses_config_without_applying_live_policy() {
        let config = br#"
[interfaces.one]
device.name = "eth0"
[interfaces.one.xdp]
zero_copy = false
workers.auto.count = 1

[interfaces.two]
device.name = "eth1"
[interfaces.two.xdp]
zero_copy = false
workers.auto.count = 1
"#;

        assert!(
            build_with_config(config, Operation::Initialize)
                .unwrap()
                .is_none()
        );
        let Err(error) = build_with_config(config, Operation::Run) else {
            panic!("live policy with two interfaces unexpectedly succeeded")
        };
        assert!(error.contains("exactly one"), "{error}");
    }

    #[test]
    fn init_rejects_invalid_config_values() {
        let config = br#"
schema_version = "one"
"#;
        let Err(error) = build_with_config(config, Operation::Initialize) else {
            panic!("invalid schema version unexpectedly succeeded")
        };
        assert!(error.contains("non-integer schema_version"), "{error}");
    }

    #[test]
    fn missing_device_is_a_targeted_error() {
        let Err(error) = resolve_xdp_device(
            "primary",
            &config_file::DeviceSelector::Name("nosuchnic0".to_string()),
        ) else {
            panic!("missing device unexpectedly resolved")
        };
        assert!(error.contains("\"nosuchnic0\""), "{error}");
    }

    #[test]
    fn ipv6_bind_is_rejected() {
        let device = NetworkDevice::new("lo").unwrap();
        let Err(error) =
            resolve_xdp_source_ipv4("primary", &device, IpAddr::V6(Ipv6Addr::LOCALHOST))
        else {
            panic!("IPv6 bind address unexpectedly accepted")
        };
        assert!(error.contains("supports IPv4 only"), "{error}");
    }
}
