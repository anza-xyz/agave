//! The `validator` module hosts all the validator microservices.

pub use solana_perf::report_target_features;
use {
    crate::{
        admin_rpc_post_init::{AdminRpcRequestMetadataPostInit, KeyUpdaterType, KeyUpdaters},
        banking_stage::{
            transaction_scheduler::scheduler_controller::SchedulerConfig,
            unified_scheduler::ensure_banking_stage_setup, BankingStage,
        },
        banking_trace::{self, BankingTracer, TraceError},
        cluster_info_vote_listener::VoteTracker,
        completed_data_sets_service::CompletedDataSetsService,
        consensus::{
            reconcile_blockstore_roots_with_external_source,
            tower_storage::{NullTowerStorage, TowerStorage},
            ExternalRootSource, Tower,
        },
        repair::{
            self,
            quic_endpoint::{RepairQuicAsyncSenders, RepairQuicSenders, RepairQuicSockets},
            repair_handler::RepairHandlerType,
            serve_repair_service::ServeRepairService,
        },
        resource_limits::{
            adjust_nofile_limit, validate_memlock_limit_for_disk_io, ResourceLimitError,
        },
        sample_performance_service::SamplePerformanceService,
        snapshot_packager_service::SnapshotPackagerService,
        stats_reporter_service::StatsReporterService,
        system_monitor_service::{
            verify_net_stats_access, SystemMonitorService, SystemMonitorStatsReportConfig,
        },
        tpu::{ForwardingClientOption, Tpu, TpuSockets},
        tvu::{Tvu, TvuConfig, TvuSockets},
    },
    agave_snapshots::{
        snapshot_archive_info::SnapshotArchiveInfoGetter as _, snapshot_config::SnapshotConfig,
        snapshot_hash::StartingSnapshotHashes, SnapshotInterval,
    },
    anyhow::{anyhow, Context, Result},
    crossbeam_channel::{bounded, unbounded, Receiver},
    quinn::Endpoint,
    serde::{Deserialize, Serialize},
    solana_account::ReadableAccount,
    solana_accounts_db::{
        accounts_db::{AccountsDbConfig, ACCOUNTS_DB_CONFIG_FOR_TESTING},
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        utils::move_and_async_delete_path_contents,
    },
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_clock::Slot,
    solana_cluster_type::ClusterType,
    solana_entry::poh::compute_hash_time,
    solana_epoch_schedule::MAX_LEADER_SCHEDULE_EPOCH_OFFSET,
    solana_genesis_config::GenesisConfig,
    solana_genesis_utils::{
        open_genesis_config, OpenGenesisConfigError, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    },
    solana_geyser_plugin_manager::{
        geyser_plugin_service::GeyserPluginService, GeyserPluginManagerRequest,
    },
    solana_gossip::{
        cluster_info::{
            ClusterInfo, DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS,
            DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS,
        },
        contact_info::ContactInfo,
        crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
        gossip_service::GossipService,
        node::{Node, NodeMultihoming},
    },
    solana_hard_forks::HardForks,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        bank_forks_utils,
        blockstore::{
            Blockstore, BlockstoreError, PurgeType, MAX_COMPLETED_SLOTS_IN_CHANNEL,
            MAX_REPLAY_WAKE_UP_SIGNALS,
        },
        blockstore_metric_report_service::BlockstoreMetricReportService,
        blockstore_options::{BlockstoreOptions, BLOCKSTORE_DIRECTORY_ROCKS_LEVEL},
        blockstore_processor::{self, TransactionStatusSender},
        entry_notifier_interface::EntryNotifierArc,
        entry_notifier_service::{EntryNotifierSender, EntryNotifierService},
        leader_schedule::FixedSchedule,
        leader_schedule_cache::LeaderScheduleCache,
        use_snapshot_archives_at_startup::UseSnapshotArchivesAtStartup,
    },
    solana_measure::measure::Measure,
    solana_metrics::{datapoint_info, metrics::metrics_config_sanity_check},
    solana_net_utils::SocketAddrSpace,
    solana_poh::{
        poh_controller::PohController,
        poh_recorder::PohRecorder,
        poh_service::{self, PohService},
        record_channels::record_channels,
        transaction_recorder::TransactionRecorder,
    },
    solana_pubkey::Pubkey,
    solana_rayon_threadlimit::get_thread_count,
    solana_rpc::{
        max_slots::MaxSlots,
        optimistically_confirmed_bank_tracker::{
            BankNotificationSenderConfig, OptimisticallyConfirmedBank,
            OptimisticallyConfirmedBankTracker,
        },
        rpc::JsonRpcConfig,
        rpc_completed_slots_service::RpcCompletedSlotsService,
        rpc_pubsub_service::{PubSubConfig, PubSubService},
        rpc_service::{JsonRpcService, JsonRpcServiceConfig, RpcTpuClientArgs},
        rpc_subscriptions::RpcSubscriptions,
        transaction_notifier_interface::TransactionNotifierArc,
        transaction_status_service::TransactionStatusService,
    },
    solana_runtime::{
        accounts_background_service::{
            AbsRequestHandlers, AccountsBackgroundService, DroppedSlotsReceiver,
            PendingSnapshotPackages, PrunedBanksRequestHandler, SnapshotRequestHandler,
        },
        bank::Bank,
        bank_forks::BankForks,
        commitment::BlockCommitmentCache,
        dependency_tracker::DependencyTracker,
        prioritization_fee_cache::PrioritizationFeeCache,
        runtime_config::RuntimeConfig,
        snapshot_bank_utils,
        snapshot_controller::SnapshotController,
        snapshot_utils::{self, clean_orphaned_account_snapshot_dirs},
    },
    solana_send_transaction_service::send_transaction_service::Config as SendTransactionServiceConfig,
    solana_shred_version::compute_shred_version,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{simple_qos::SimpleQosConfig, swqos::SwQosConfig},
        quic::{QuicStreamerConfig, SimpleQosQuicStreamerConfig, SwQosQuicStreamerConfig},
        streamer::StakedNodes,
    },
    solana_time_utils::timestamp,
    solana_tpu_client::tpu_client::{DEFAULT_TPU_CONNECTION_POOL_SIZE, DEFAULT_VOTE_USE_QUIC},
    solana_turbine::{
        self,
        broadcast_stage::BroadcastStageType,
        xdp::{master_ip_if_bonded, XdpConfig, XdpRetransmitter},
    },
    solana_unified_scheduler_logic::SchedulingMode,
    solana_unified_scheduler_pool::{DefaultSchedulerPool, SupportedSchedulingMode},
    solana_validator_exit::Exit,
    solana_vote_program::vote_state::VoteStateV4,
    solana_wen_restart::wen_restart::{wait_for_wen_restart, WenRestartConfig},
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        net::{IpAddr, SocketAddr},
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    strum::VariantNames,
    strum_macros::{Display, EnumCount, EnumIter, EnumString, EnumVariantNames, IntoStaticStr},
    thiserror::Error,
    tokio::{runtime::Runtime as TokioRuntime, sync::mpsc},
    tokio_util::sync::CancellationToken,
};

const MAX_COMPLETED_DATA_SETS_IN_CHANNEL: usize = 100_000;
const WAIT_FOR_SUPERMAJORITY_THRESHOLD_PERCENT: u64 = 80;
// Right now since we reuse the wait for supermajority code, the
// following threshold should always greater than or equal to
// WAIT_FOR_SUPERMAJORITY_THRESHOLD_PERCENT.
const WAIT_FOR_WEN_RESTART_SUPERMAJORITY_THRESHOLD_PERCENT: u64 =
    WAIT_FOR_SUPERMAJORITY_THRESHOLD_PERCENT;

#[derive(
    Clone, EnumCount, EnumIter, EnumString, EnumVariantNames, Default, IntoStaticStr, Display,
)]
#[strum(serialize_all = "kebab-case")]
pub enum BlockVerificationMethod {
    BlockstoreProcessor,
    #[default]
    UnifiedScheduler,
}

impl BlockVerificationMethod {
    pub const fn cli_names() -> &'static [&'static str] {
        Self::VARIANTS
    }

    pub fn cli_message() -> &'static str {
        "Switch transaction scheduling method for verifying ledger entries"
    }
}

#[derive(
    Clone,
    Debug,
    EnumCount,
    EnumIter,
    EnumString,
    EnumVariantNames,
    Default,
    IntoStaticStr,
    Display,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum BlockProductionMethod {
    CentralScheduler,
    #[default]
    CentralSchedulerGreedy,
    UnifiedScheduler,
}

impl BlockProductionMethod {
    pub fn cli_names() -> &'static [&'static str] {
        // Simply return Self::VARIANTS by removing this code block altogether once after
        // UnifiedScheduler isn't experimental
        {
            use std::sync::LazyLock;
            static VARIANTS_NO_EXPERIMENTAL: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
                BlockProductionMethod::VARIANTS
                    .iter()
                    .filter_map(|&variant| (variant != "unified-scheduler").then_some(variant))
                    .collect()
            });

            let disable_experimental =
                std::env::var("SOLANA_ENABLE_EXPERIMENTAL_BLOCK_PRODUCTION_METHOD").is_err();
            if disable_experimental {
                return &VARIANTS_NO_EXPERIMENTAL[..];
            }
        }

        Self::VARIANTS
    }

    pub fn cli_message() -> &'static str {
        "Switch transaction scheduling method for producing ledger entries"
    }
}

#[derive(
    Clone,
    Debug,
    EnumString,
    EnumVariantNames,
    Default,
    IntoStaticStr,
    Display,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum TransactionStructure {
    Sdk,
    #[default]
    View,
}

impl TransactionStructure {
    pub const fn cli_names() -> &'static [&'static str] {
        Self::VARIANTS
    }

    pub fn cli_message() -> &'static str {
        "DEPRECATED: has no impact on banking stage; will be removed in a future version"
    }
}

#[derive(
    Clone, Debug, EnumVariantNames, IntoStaticStr, Display, Serialize, Deserialize, PartialEq, Eq,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum SchedulerPacing {
    Disabled,
    FillTimeMillis(NonZeroU64),
}

impl FromStr for SchedulerPacing {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("disabled") {
            Ok(SchedulerPacing::Disabled)
        } else {
            match s.parse::<u64>() {
                Ok(v) if v > 0 => Ok(SchedulerPacing::FillTimeMillis(
                    NonZeroU64::new(v).ok_or_else(|| "value must be non-zero".to_string())?,
                )),
                _ => Err("value must be a positive integer or 'disabled'".to_string()),
            }
        }
    }
}

impl SchedulerPacing {
    pub fn fill_time(&self) -> Option<Duration> {
        match self {
            SchedulerPacing::Disabled => None,
            SchedulerPacing::FillTimeMillis(millis) => Some(Duration::from_millis(millis.get())),
        }
    }
}

pub fn supported_scheduling_mode(
    (verification, production): (&BlockVerificationMethod, &BlockProductionMethod),
) -> SupportedSchedulingMode {
    match (verification, production) {
        (BlockVerificationMethod::UnifiedScheduler, BlockProductionMethod::UnifiedScheduler) => {
            SupportedSchedulingMode::Both
        }
        (BlockVerificationMethod::UnifiedScheduler, _) => {
            SupportedSchedulingMode::Either(SchedulingMode::BlockVerification)
        }
        (_, BlockProductionMethod::UnifiedScheduler) => {
            SupportedSchedulingMode::Either(SchedulingMode::BlockProduction)
        }
        _ => unreachable!("seems unified scheduler is disabled"),
    }
}

/// Configuration for the block generator invalidator for replay.
#[derive(Clone, Debug)]
pub struct GeneratorConfig {
    pub accounts_path: String,
    pub starting_keypairs: Arc<Vec<Keypair>>,
}

pub struct ValidatorConfig {
    /// The destination file for validator logs; `stderr` is used if `None`
    pub logfile: Option<PathBuf>,
    pub expected_genesis_hash: Option<Hash>,
    pub expected_bank_hash: Option<Hash>,
    pub expected_shred_version: Option<u16>,
    pub voting_disabled: bool,
    pub account_paths: Vec<PathBuf>,
    pub account_snapshot_paths: Vec<PathBuf>,
    pub rpc_config: JsonRpcConfig,
    /// Specifies which plugins to start up with
    pub on_start_geyser_plugin_config_files: Option<Vec<PathBuf>>,
    pub geyser_plugin_always_enabled: bool,
    pub rpc_addrs: Option<(SocketAddr, SocketAddr)>, // (JsonRpc, JsonRpcPubSub)
    pub pubsub_config: PubSubConfig,
    pub snapshot_config: SnapshotConfig,
    pub max_ledger_shreds: Option<u64>,
    pub blockstore_options: BlockstoreOptions,
    pub broadcast_stage_type: BroadcastStageType,
    pub turbine_disabled: Arc<AtomicBool>,
    pub fixed_leader_schedule: Option<FixedSchedule>,
    pub wait_for_supermajority: Option<Slot>,
    pub new_hard_forks: Option<Vec<Slot>>,
    pub known_validators: Option<HashSet<Pubkey>>, // None = trust all
    pub repair_validators: Option<HashSet<Pubkey>>, // None = repair from all
    pub repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>, // Empty = repair with all
    pub gossip_validators: Option<HashSet<Pubkey>>, // None = gossip with all
    pub max_genesis_archive_unpacked_size: u64,
    /// Run PoH, transaction signature and other transaction verification during blockstore
    /// processing.
    pub run_verification: bool,
    pub require_tower: bool,
    pub tower_storage: Arc<dyn TowerStorage>,
    pub debug_keys: Option<Arc<HashSet<Pubkey>>>,
    pub contact_debug_interval: u64,
    pub contact_save_interval: u64,
    pub send_transaction_service_config: SendTransactionServiceConfig,
    pub no_poh_speed_test: bool,
    pub no_os_memory_stats_reporting: bool,
    pub no_os_network_stats_reporting: bool,
    pub no_os_cpu_stats_reporting: bool,
    pub no_os_disk_stats_reporting: bool,
    pub enforce_ulimit_nofile: bool,
    pub poh_pinned_cpu_core: usize,
    pub poh_hashes_per_batch: u64,
    pub process_ledger_before_services: bool,
    pub accounts_db_config: AccountsDbConfig,
    pub warp_slot: Option<Slot>,
    pub accounts_db_skip_shrink: bool,
    pub accounts_db_force_initial_clean: bool,
    pub staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub validator_exit_backpressure: HashMap<String, Arc<AtomicBool>>,
    pub no_wait_for_vote_to_start_leader: bool,
    pub wait_to_vote_slot: Option<Slot>,
    pub runtime_config: RuntimeConfig,
    pub banking_trace_dir_byte_limit: banking_trace::DirByteLimit,
    pub block_verification_method: BlockVerificationMethod,
    pub block_production_method: BlockProductionMethod,
    pub block_production_num_workers: NonZeroUsize,
    pub block_production_scheduler_config: SchedulerConfig,
    pub enable_block_production_forwarding: bool,
    pub enable_scheduler_bindings: bool,
    pub generator_config: Option<GeneratorConfig>,
    pub use_snapshot_archives_at_startup: UseSnapshotArchivesAtStartup,
    pub wen_restart_proto_path: Option<PathBuf>,
    pub wen_restart_coordinator: Option<Pubkey>,
    pub unified_scheduler_handler_threads: Option<usize>,
    pub ip_echo_server_threads: NonZeroUsize,
    pub rayon_global_threads: NonZeroUsize,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
    pub tvu_shred_sigverify_threads: NonZeroUsize,
    pub delay_leader_block_for_pending_fork: bool,
    pub retransmit_xdp: Option<XdpConfig>,
    pub repair_handler_type: RepairHandlerType,
}

impl ValidatorConfig {
    pub fn default_for_test() -> Self {
        let max_thread_count =
            NonZeroUsize::new(num_cpus::get()).expect("thread count is non-zero");

        Self {
            logfile: None,
            expected_genesis_hash: None,
            expected_bank_hash: None,
            expected_shred_version: None,
            voting_disabled: false,
            max_ledger_shreds: None,
            blockstore_options: BlockstoreOptions::default_for_tests(),
            account_paths: Vec::new(),
            account_snapshot_paths: Vec::new(),
            rpc_config: JsonRpcConfig::default_for_test(),
            on_start_geyser_plugin_config_files: None,
            geyser_plugin_always_enabled: false,
            rpc_addrs: None,
            pubsub_config: PubSubConfig::default(),
            snapshot_config: SnapshotConfig::new_load_only(),
            broadcast_stage_type: BroadcastStageType::Standard,
            turbine_disabled: Arc::<AtomicBool>::default(),
            fixed_leader_schedule: None,
            wait_for_supermajority: None,
            new_hard_forks: None,
            known_validators: None,
            repair_validators: None,
            repair_whitelist: Arc::new(RwLock::new(HashSet::default())),
            gossip_validators: None,
            max_genesis_archive_unpacked_size: MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            run_verification: true,
            require_tower: false,
            tower_storage: Arc::new(NullTowerStorage::default()),
            debug_keys: None,
            contact_debug_interval: DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS,
            contact_save_interval: DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS,
            send_transaction_service_config: SendTransactionServiceConfig::default(),
            no_poh_speed_test: true,
            no_os_memory_stats_reporting: true,
            no_os_network_stats_reporting: true,
            no_os_cpu_stats_reporting: true,
            no_os_disk_stats_reporting: true,
            // No need to enforce nofile limit in tests
            enforce_ulimit_nofile: false,
            poh_pinned_cpu_core: poh_service::DEFAULT_PINNED_CPU_CORE,
            poh_hashes_per_batch: poh_service::DEFAULT_HASHES_PER_BATCH,
            process_ledger_before_services: false,
            warp_slot: None,
            accounts_db_skip_shrink: false,
            accounts_db_force_initial_clean: false,
            staked_nodes_overrides: Arc::new(RwLock::new(HashMap::new())),
            validator_exit: Arc::new(RwLock::new(Exit::default())),
            validator_exit_backpressure: HashMap::default(),
            no_wait_for_vote_to_start_leader: true,
            accounts_db_config: ACCOUNTS_DB_CONFIG_FOR_TESTING,
            wait_to_vote_slot: None,
            runtime_config: RuntimeConfig::default(),
            banking_trace_dir_byte_limit: 0,
            block_verification_method: BlockVerificationMethod::default(),
            block_production_method: BlockProductionMethod::default(),
            block_production_num_workers: BankingStage::default_num_workers(),
            block_production_scheduler_config: SchedulerConfig::default(),
            // enable forwarding by default for tests
            enable_block_production_forwarding: true,
            enable_scheduler_bindings: false,
            generator_config: None,
            use_snapshot_archives_at_startup: UseSnapshotArchivesAtStartup::default(),
            wen_restart_proto_path: None,
            wen_restart_coordinator: None,
            unified_scheduler_handler_threads: None,
            ip_echo_server_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            rayon_global_threads: max_thread_count,
            replay_forks_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            replay_transactions_threads: max_thread_count,
            tvu_shred_sigverify_threads: NonZeroUsize::new(get_thread_count())
                .expect("thread count is non-zero"),
            delay_leader_block_for_pending_fork: false,
            retransmit_xdp: None,
            repair_handler_type: RepairHandlerType::default(),
        }
    }

    pub fn enable_default_rpc_block_subscribe(&mut self) {
        let pubsub_config = PubSubConfig {
            enable_block_subscription: true,
            ..PubSubConfig::default()
        };
        let rpc_config = JsonRpcConfig {
            enable_rpc_transaction_history: true,
            ..JsonRpcConfig::default_for_test()
        };

        self.pubsub_config = pubsub_config;
        self.rpc_config = rpc_config;
    }
}

// `ValidatorStartProgress` contains status information that is surfaced to the node operator over
// the admin RPC channel to help them to follow the general progress of node startup without
// having to watch log messages.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ValidatorStartProgress {
    #[default]
    Initializing, // Catch all, default state
    SearchingForRpcService,
    DownloadingSnapshot {
        slot: Slot,
        rpc_addr: SocketAddr,
    },
    CleaningBlockStore,
    CleaningAccounts,
    LoadingLedger,
    ProcessingLedger {
        slot: Slot,
        max_slot: Slot,
    },
    StartingServices,
    // This case corresponds to a state that is entered by using the now
    // deprecated `--dev-halt-at-slot` flag. A different version of the
    // validator may be used to monitor a running validator so leave the case
    // here to avoid any compatibility concerns
    Halted,
    WaitingForSupermajority {
        slot: Slot,
        gossip_stake_percent: u64,
    },

    // `Running` is the terminal state once the validator fully starts and all services are
    // operational
    Running,
}

struct BlockstoreRootScan {
    thread: Option<JoinHandle<Result<usize, BlockstoreError>>>,
}

impl BlockstoreRootScan {
    fn new(config: &ValidatorConfig, blockstore: Arc<Blockstore>, exit: Arc<AtomicBool>) -> Self {
        let thread = if config.rpc_addrs.is_some()
            && config.rpc_config.enable_rpc_transaction_history
            && config.rpc_config.rpc_scan_and_fix_roots
        {
            Some(
                Builder::new()
                    .name("solBStoreRtScan".to_string())
                    .spawn(move || blockstore.scan_and_fix_roots(None, None, &exit))
                    .unwrap(),
            )
        } else {
            None
        };
        Self { thread }
    }

    fn join(self) {
        if let Some(blockstore_root_scan) = self.thread {
            if let Err(err) = blockstore_root_scan.join() {
                warn!("blockstore_root_scan failed to join {err:?}");
            }
        }
    }
}

#[derive(Default)]
struct TransactionHistoryServices {
    transaction_status_sender: Option<TransactionStatusSender>,
    transaction_status_service: Option<TransactionStatusService>,
    max_complete_transaction_status_slot: Arc<AtomicU64>,
}

/// A struct easing passing Validator TPU Configurations
pub struct ValidatorTpuConfig {
    /// Controls if to use QUIC for sending TPU votes
    pub vote_use_quic: bool,
    /// Controls the connection cache pool size
    pub tpu_connection_pool_size: usize,
    /// Controls if to enable UDP for TPU transactions.
    pub tpu_enable_udp: bool,
    /// QUIC server config for regular TPU
    pub tpu_quic_server_config: SwQosQuicStreamerConfig,
    /// QUIC server config for TPU forward
    pub tpu_fwd_quic_server_config: SwQosQuicStreamerConfig,
    /// QUIC server config for Vote
    pub vote_quic_server_config: SimpleQosQuicStreamerConfig,
}

impl ValidatorTpuConfig {
    /// A convenient function to build a ValidatorTpuConfig for testing with good
    /// default.
    pub fn new_for_tests(tpu_enable_udp: bool) -> Self {
        let tpu_quic_server_config = SwQosQuicStreamerConfig {
            quic_streamer_config: QuicStreamerConfig {
                max_connections_per_ipaddr_per_min: 32,
                ..Default::default()
            },
            qos_config: SwQosConfig::default(),
        };

        let tpu_fwd_quic_server_config = SwQosQuicStreamerConfig {
            quic_streamer_config: QuicStreamerConfig {
                max_connections_per_ipaddr_per_min: 32,
                ..Default::default()
            },
            qos_config: SwQosConfig {
                max_unstaked_connections: 0,
                ..Default::default()
            },
        };

        // vote and tpu_fwd share the same characteristics -- disallow non-staked connections:
        let vote_quic_server_config = SimpleQosQuicStreamerConfig {
            quic_streamer_config: QuicStreamerConfig {
                max_connections_per_ipaddr_per_min: 32,
                ..Default::default()
            },
            qos_config: SimpleQosConfig::default(),
        };

        ValidatorTpuConfig {
            vote_use_quic: DEFAULT_VOTE_USE_QUIC,
            tpu_connection_pool_size: DEFAULT_TPU_CONNECTION_POOL_SIZE,
            tpu_enable_udp,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
        }
    }
}

pub struct Validator {
    /// The destination file for validator logs; `stderr` is used if `None`
    #[cfg_attr(not(unix), allow(dead_code))]
    logfile: Option<PathBuf>,
    /// A global flag to indicate communicate shutdown between threads
    exit: Arc<AtomicBool>,
    validator_exit: Arc<RwLock<Exit>>,
    json_rpc_service: Option<JsonRpcService>,
    pubsub_service: Option<PubSubService>,
    rpc_completed_slots_service: Option<JoinHandle<()>>,
    optimistically_confirmed_bank_tracker: Option<OptimisticallyConfirmedBankTracker>,
    transaction_status_service: Option<TransactionStatusService>,
    entry_notifier_service: Option<EntryNotifierService>,
    system_monitor_service: Option<SystemMonitorService>,
    sample_performance_service: Option<SamplePerformanceService>,
    stats_reporter_service: StatsReporterService,
    gossip_service: GossipService,
    serve_repair_service: ServeRepairService,
    completed_data_sets_service: Option<CompletedDataSetsService>,
    snapshot_packager_service: Option<SnapshotPackagerService>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    poh_service: PohService,
    tpu: Tpu,
    tvu: Tvu,
    ip_echo_server: Option<solana_net_utils::IpEchoServer>,
    pub cluster_info: Arc<ClusterInfo>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
    geyser_plugin_service: Option<GeyserPluginService>,
    blockstore_metric_report_service: BlockstoreMetricReportService,
    accounts_background_service: AccountsBackgroundService,
    turbine_quic_endpoint: Option<Endpoint>,
    turbine_quic_endpoint_runtime: Option<TokioRuntime>,
    turbine_quic_endpoint_join_handle: Option<solana_turbine::quic_endpoint::AsyncTryJoinHandle>,
    repair_quic_endpoints: Option<[Endpoint; 3]>,
    repair_quic_endpoints_runtime: Option<TokioRuntime>,
    repair_quic_endpoints_join_handle: Option<repair::quic_endpoint::AsyncTryJoinHandle>,
    xdp_retransmitter: Option<XdpRetransmitter>,
    // This runtime is used to run the client owned by SendTransactionService.
    // We don't wait for its JoinHandle here because ownership and shutdown
    // are managed elsewhere. This variable is intentionally unused.
    _tpu_client_next_runtime: Option<TokioRuntime>,
}

impl Validator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut node: Node,
        identity_keypair: Arc<Keypair>,
        ledger_path: &Path,
        vote_account: &Pubkey,
        authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
        cluster_entrypoints: Vec<ContactInfo>,
        config: &ValidatorConfig,
        should_check_duplicate_instance: bool,
        rpc_to_plugin_manager_receiver: Option<Receiver<GeyserPluginManagerRequest>>,
        start_progress: Arc<RwLock<ValidatorStartProgress>>,
        socket_addr_space: SocketAddrSpace,
        tpu_config: ValidatorTpuConfig,
        admin_rpc_service_post_init: Arc<RwLock<Option<AdminRpcRequestMetadataPostInit>>>,
    ) -> Result<Self> {
        #[cfg(debug_assertions)]
        const DEBUG_ASSERTION_STATUS: &str = "enabled";
        #[cfg(not(debug_assertions))]
        const DEBUG_ASSERTION_STATUS: &str = "disabled";
        info!("debug-assertion status: {DEBUG_ASSERTION_STATUS}");

        let ValidatorTpuConfig {
            vote_use_quic,
            tpu_connection_pool_size,
            tpu_enable_udp,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
        } = tpu_config;

        let start_time = Instant::now();

        adjust_nofile_limit(config.enforce_ulimit_nofile)?;

        // Initialize the global rayon pool first to ensure the value in config
        // is honored. Otherwise, some code accessing the global pool could
        // cause it to get initialized with Rayon's default (not ours)
        if rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("solRayonGlob{i:02}"))
            .num_threads(config.rayon_global_threads.get())
            .build_global()
            .is_err()
        {
            warn!("Rayon global thread pool already initialized");
        }

        let id = identity_keypair.pubkey();
        assert_eq!(&id, node.info.pubkey());

        info!("identity pubkey: {id}");
        info!("vote account pubkey: {vote_account}");

        if !config.no_os_network_stats_reporting {
            verify_net_stats_access().map_err(|e| {
                ValidatorError::Other(format!("Failed to access network stats: {e:?}"))
            })?;
        }

        let mut bank_notification_senders = Vec::new();

        let exit = Arc::new(AtomicBool::new(false));

        let geyser_plugin_config_files = config
            .on_start_geyser_plugin_config_files
            .as_ref()
            .map(Cow::Borrowed)
            .or_else(|| {
                config
                    .geyser_plugin_always_enabled
                    .then_some(Cow::Owned(vec![]))
            });
        let geyser_plugin_service =
            if let Some(geyser_plugin_config_files) = geyser_plugin_config_files {
                let (confirmed_bank_sender, confirmed_bank_receiver) = unbounded();
                bank_notification_senders.push(confirmed_bank_sender);
                let rpc_to_plugin_manager_receiver_and_exit =
                    rpc_to_plugin_manager_receiver.map(|receiver| (receiver, exit.clone()));
                Some(
                    GeyserPluginService::new_with_receiver(
                        confirmed_bank_receiver,
                        config.geyser_plugin_always_enabled,
                        geyser_plugin_config_files.as_ref(),
                        rpc_to_plugin_manager_receiver_and_exit,
                    )
                    .map_err(|err| {
                        ValidatorError::Other(format!("Failed to load the Geyser plugin: {err:?}"))
                    })?,
                )
            } else {
                None
            };

        if config.voting_disabled {
            warn!("voting disabled");
            authorized_voter_keypairs.write().unwrap().clear();
        } else {
            for authorized_voter_keypair in authorized_voter_keypairs.read().unwrap().iter() {
                warn!("authorized voter: {}", authorized_voter_keypair.pubkey());
            }
        }

        for cluster_entrypoint in &cluster_entrypoints {
            info!("entrypoint: {cluster_entrypoint:?}");
        }

        validate_memlock_limit_for_disk_io(config.accounts_db_config.memlock_budget_size)?;

        if !ledger_path.is_dir() {
            return Err(anyhow!(
                "ledger directory does not exist or is not accessible: {ledger_path:?}"
            ));
        }
        let genesis_config = load_genesis(config, ledger_path)?;
        metrics_config_sanity_check(genesis_config.cluster_type)?;

        info!("Cleaning accounts paths..");
        *start_progress.write().unwrap() = ValidatorStartProgress::CleaningAccounts;
        let mut timer = Measure::start("clean_accounts_paths");
        cleanup_accounts_paths(config);
        timer.stop();
        info!("Cleaning accounts paths done. {timer}");

        snapshot_utils::purge_incomplete_bank_snapshots(&config.snapshot_config.bank_snapshots_dir);
        snapshot_utils::purge_old_bank_snapshots_at_startup(
            &config.snapshot_config.bank_snapshots_dir,
        );

        info!("Cleaning orphaned account snapshot directories..");
        let mut timer = Measure::start("clean_orphaned_account_snapshot_dirs");
        clean_orphaned_account_snapshot_dirs(
            &config.snapshot_config.bank_snapshots_dir,
            &config.account_snapshot_paths,
        )
        .context("failed to clean orphaned account snapshot directories")?;
        timer.stop();
        info!("Cleaning orphaned account snapshot directories done. {timer}");

        // token used to cancel tpu-client-next and streamer.
        let cancel = CancellationToken::new();
        {
            let exit = exit.clone();
            config
                .validator_exit
                .write()
                .unwrap()
                .register_exit(Box::new(move || exit.store(true, Ordering::Relaxed)));
            let cancel = cancel.clone();
            config
                .validator_exit
                .write()
                .unwrap()
                .register_exit(Box::new(move || cancel.cancel()));
        }

        let (
            accounts_update_notifier,
            transaction_notifier,
            entry_notifier,
            block_metadata_notifier,
            slot_status_notifier,
        ) = if let Some(service) = &geyser_plugin_service {
            (
                service.get_accounts_update_notifier(),
                service.get_transaction_notifier(),
                service.get_entry_notifier(),
                service.get_block_metadata_notifier(),
                service.get_slot_status_notifier(),
            )
        } else {
            (None, None, None, None, None)
        };

        info!(
            "Geyser plugin: accounts_update_notifier: {}, transaction_notifier: {}, \
             entry_notifier: {}",
            accounts_update_notifier.is_some(),
            transaction_notifier.is_some(),
            entry_notifier.is_some()
        );

        let system_monitor_service = Some(SystemMonitorService::new(
            exit.clone(),
            SystemMonitorStatsReportConfig {
                report_os_memory_stats: !config.no_os_memory_stats_reporting,
                report_os_network_stats: !config.no_os_network_stats_reporting,
                report_os_cpu_stats: !config.no_os_cpu_stats_reporting,
                report_os_disk_stats: !config.no_os_disk_stats_reporting,
            },
        ));

        let dependency_tracker = Arc::new(DependencyTracker::default());

        let (
            bank_forks,
            blockstore,
            original_blockstore_root,
            ledger_signal_receiver,
            leader_schedule_cache,
            starting_snapshot_hashes,
            TransactionHistoryServices {
                transaction_status_sender,
                transaction_status_service,
                max_complete_transaction_status_slot,
            },
            blockstore_process_options,
            blockstore_root_scan,
            pruned_banks_receiver,
            entry_notifier_service,
        ) = load_blockstore(
            config,
            ledger_path,
            &genesis_config,
            exit.clone(),
            &start_progress,
            accounts_update_notifier,
            transaction_notifier,
            entry_notifier,
            config
                .rpc_addrs
                .is_some()
                .then(|| dependency_tracker.clone()),
        )
        .map_err(ValidatorError::Other)?;

        let migration_status = bank_forks.read().unwrap().migration_status();

        if !config.no_poh_speed_test {
            check_poh_speed(&bank_forks.read().unwrap().root_bank(), None)?;
        }

        let (root_slot, hard_forks) = {
            let root_bank = bank_forks.read().unwrap().root_bank();
            (root_bank.slot(), root_bank.hard_forks())
        };
        let shred_version = compute_shred_version(&genesis_config.hash(), Some(&hard_forks));
        info!("shred version: {shred_version}, hard forks: {hard_forks:?}");

        if let Some(expected_shred_version) = config.expected_shred_version {
            if expected_shred_version != shred_version {
                return Err(ValidatorError::ShredVersionMismatch {
                    actual: shred_version,
                    expected: expected_shred_version,
                }
                .into());
            }
        }

        if let Some(start_slot) = should_cleanup_blockstore_incorrect_shred_versions(
            config,
            &blockstore,
            root_slot,
            &hard_forks,
        )? {
            *start_progress.write().unwrap() = ValidatorStartProgress::CleaningBlockStore;
            cleanup_blockstore_incorrect_shred_versions(
                &blockstore,
                config,
                start_slot,
                shred_version,
            )?;
        } else {
            info!("Skipping the blockstore check for shreds with incorrect version");
        }

        node.info.set_shred_version(shred_version);
        node.info.set_wallclock(timestamp());
        Self::print_node_info(&node);

        let mut cluster_info = ClusterInfo::new(
            node.info.clone(),
            identity_keypair.clone(),
            socket_addr_space,
        );
        cluster_info.set_contact_debug_interval(config.contact_debug_interval);
        cluster_info.set_entrypoints(cluster_entrypoints);
        cluster_info.restore_contact_info(ledger_path, config.contact_save_interval);
        cluster_info.set_bind_ip_addrs(node.bind_ip_addrs.clone());
        let cluster_info = Arc::new(cluster_info);
        let node_multihoming = Arc::new(NodeMultihoming::from(&node));
        migration_status.set_pubkey(cluster_info.id());

        assert!(is_snapshot_config_valid(&config.snapshot_config));

        let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
        let snapshot_controller = Arc::new(SnapshotController::new(
            snapshot_request_sender.clone(),
            config.snapshot_config.clone(),
            bank_forks.read().unwrap().root(),
        ));

        let pending_snapshot_packages = Arc::new(Mutex::new(PendingSnapshotPackages::default()));
        let snapshot_packager_service = if snapshot_controller
            .snapshot_config()
            .should_generate_snapshots()
        {
            let exit_backpressure = config
                .validator_exit_backpressure
                .get(SnapshotPackagerService::NAME)
                .cloned();
            let enable_gossip_push = true;
            let snapshot_packager_service = SnapshotPackagerService::new(
                pending_snapshot_packages.clone(),
                starting_snapshot_hashes,
                exit.clone(),
                exit_backpressure,
                cluster_info.clone(),
                snapshot_controller.clone(),
                enable_gossip_push,
            );
            Some(snapshot_packager_service)
        } else {
            None
        };

        let snapshot_request_handler = SnapshotRequestHandler {
            snapshot_controller: snapshot_controller.clone(),
            snapshot_request_receiver,
            pending_snapshot_packages,
        };
        let pruned_banks_request_handler = PrunedBanksRequestHandler {
            pruned_banks_receiver,
        };
        let accounts_background_service = AccountsBackgroundService::new(
            bank_forks.clone(),
            exit.clone(),
            AbsRequestHandlers {
                snapshot_request_handler,
                pruned_banks_request_handler,
            },
        );
        info!(
            "Using: block-verification-method: {}, block-production-method: {}",
            config.block_verification_method, config.block_production_method,
        );

        let (replay_vote_sender, replay_vote_receiver) = unbounded();

        let prioritization_fee_cache = if config.rpc_config.full_api {
            Some(Arc::new(PrioritizationFeeCache::default()))
        } else {
            None
        };

        let leader_schedule_cache = Arc::new(leader_schedule_cache);
        let (poh_recorder, entry_receiver) = {
            let bank = &bank_forks.read().unwrap().working_bank();
            PohRecorder::new_with_clear_signal(
                bank.tick_height(),
                bank.last_blockhash(),
                bank.clone(),
                None,
                bank.ticks_per_slot(),
                config.delay_leader_block_for_pending_fork,
                blockstore.clone(),
                blockstore.get_new_shred_signal(0),
                &leader_schedule_cache,
                &genesis_config.poh_config,
                exit.clone(),
            )
        };
        let (record_sender, record_receiver) = record_channels(transaction_status_sender.is_some());
        let transaction_recorder = TransactionRecorder::new(record_sender);
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let (poh_controller, poh_service_message_receiver) = PohController::new();

        let (banking_tracer, tracer_thread) =
            BankingTracer::new((config.banking_trace_dir_byte_limit > 0).then_some((
                &blockstore.banking_trace_path(),
                exit.clone(),
                config.banking_trace_dir_byte_limit,
            )))?;
        if banking_tracer.is_enabled() {
            info!(
                "Enabled banking trace (dir_byte_limit: {})",
                config.banking_trace_dir_byte_limit
            );
        } else {
            info!("Disabled banking trace");
        }
        let banking_tracer_channels = banking_tracer.create_channels();

        match (
            &config.block_verification_method,
            &config.block_production_method,
        ) {
            methods @ (BlockVerificationMethod::UnifiedScheduler, _)
            | methods @ (_, BlockProductionMethod::UnifiedScheduler) => {
                let scheduler_pool = DefaultSchedulerPool::new(
                    supported_scheduling_mode(methods),
                    config.unified_scheduler_handler_threads,
                    config.runtime_config.log_messages_bytes_limit,
                    transaction_status_sender.clone(),
                    Some(replay_vote_sender.clone()),
                    prioritization_fee_cache.clone(),
                );
                ensure_banking_stage_setup(
                    &scheduler_pool,
                    &bank_forks,
                    &banking_tracer_channels,
                    &poh_recorder,
                    transaction_recorder.clone(),
                    config.block_production_num_workers,
                );
                bank_forks
                    .write()
                    .unwrap()
                    .install_scheduler_pool(scheduler_pool);
            }
            _ => {
                info!("no scheduler pool is installed for block verification/production...");
                if let Some(count) = config.unified_scheduler_handler_threads {
                    warn!(
                        "--unified-scheduler-handler-threads={count} is ignored because unified \
                         scheduler isn't enabled"
                    );
                }
            }
        }

        let entry_notification_sender = entry_notifier_service
            .as_ref()
            .map(|service| service.sender());
        let mut process_blockstore = ProcessBlockStore::new(
            &id,
            vote_account,
            &start_progress,
            &blockstore,
            original_blockstore_root,
            &bank_forks,
            &leader_schedule_cache,
            &blockstore_process_options,
            transaction_status_sender.as_ref(),
            entry_notification_sender,
            blockstore_root_scan,
            &snapshot_controller,
            config,
        );

        maybe_warp_slot(
            config,
            &mut process_blockstore,
            ledger_path,
            &bank_forks,
            &leader_schedule_cache,
            &snapshot_controller,
        )
        .map_err(ValidatorError::Other)?;

        if config.process_ledger_before_services {
            process_blockstore
                .process()
                .map_err(ValidatorError::Other)?;
        }
        *start_progress.write().unwrap() = ValidatorStartProgress::StartingServices;

        let sample_performance_service =
            if config.rpc_addrs.is_some() && config.rpc_config.enable_rpc_transaction_history {
                Some(SamplePerformanceService::new(
                    &bank_forks,
                    blockstore.clone(),
                    exit.clone(),
                ))
            } else {
                None
            };

        let mut block_commitment_cache = BlockCommitmentCache::default();
        let bank_forks_guard = bank_forks.read().unwrap();
        block_commitment_cache.initialize_slots(
            bank_forks_guard.working_bank().slot(),
            bank_forks_guard.root(),
        );
        drop(bank_forks_guard);
        let block_commitment_cache = Arc::new(RwLock::new(block_commitment_cache));

        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);

        let max_slots = Arc::new(MaxSlots::default());

        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let mut tpu_transactions_forwards_client_sockets =
            Some(node.sockets.tpu_transaction_forwarding_clients);

        let vote_connection_cache = if vote_use_quic {
            let vote_connection_cache = ConnectionCache::new_with_client_options(
                "connection_cache_vote_quic",
                tpu_connection_pool_size,
                Some(node.sockets.quic_vote_client),
                Some((
                    &identity_keypair,
                    node.info
                        .tpu_vote(Protocol::QUIC)
                        .ok_or_else(|| {
                            ValidatorError::Other(String::from("Invalid QUIC address for TPU Vote"))
                        })?
                        .ip(),
                )),
                Some((&staked_nodes, &identity_keypair.pubkey())),
            );
            Arc::new(vote_connection_cache)
        } else {
            Arc::new(ConnectionCache::with_udp(
                "connection_cache_vote_udp",
                tpu_connection_pool_size,
            ))
        };

        // test-validator crate may start the validator in a tokio runtime
        // context which forces us to use the same runtime because a nested
        // runtime will cause panic at drop. Outside test-validator crate, we
        // always need a tokio runtime (and the respective handle) to initialize
        // the turbine QUIC endpoint.
        let current_runtime_handle = tokio::runtime::Handle::try_current();
        let tpu_client_next_runtime = current_runtime_handle.is_err().then(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("solTpuClientRt")
                .build()
                .unwrap()
        });

        let rpc_override_health_check =
            Arc::new(AtomicBool::new(config.rpc_config.disable_health_check));
        let (
            json_rpc_service,
            rpc_subscriptions,
            pubsub_service,
            completed_data_sets_sender,
            completed_data_sets_service,
            rpc_completed_slots_service,
            optimistically_confirmed_bank_tracker,
            bank_notification_sender,
        ) = if let Some((rpc_addr, rpc_pubsub_addr)) = config.rpc_addrs {
            assert_eq!(
                node.info.rpc().map(|addr| socket_addr_space.check(&addr)),
                node.info
                    .rpc_pubsub()
                    .map(|addr| socket_addr_space.check(&addr))
            );
            let (bank_notification_sender, bank_notification_receiver) = unbounded();
            let confirmed_bank_subscribers = if !bank_notification_senders.is_empty() {
                Some(Arc::new(RwLock::new(bank_notification_senders)))
            } else {
                None
            };

            let rpc_tpu_client_args = {
                let runtime_handle = tpu_client_next_runtime
                    .as_ref()
                    .map(TokioRuntime::handle)
                    .unwrap_or_else(|| current_runtime_handle.as_ref().unwrap());

                RpcTpuClientArgs(
                    Arc::as_ref(&identity_keypair),
                    node.sockets.rpc_sts_client,
                    runtime_handle.clone(),
                    cancel.clone(),
                )
            };
            let rpc_svc_config = JsonRpcServiceConfig {
                rpc_addr,
                rpc_config: config.rpc_config.clone(),
                snapshot_config: Some(snapshot_controller.snapshot_config().clone()),
                bank_forks: bank_forks.clone(),
                block_commitment_cache: block_commitment_cache.clone(),
                blockstore: blockstore.clone(),
                cluster_info: cluster_info.clone(),
                poh_recorder: Some(poh_recorder.clone()),
                genesis_hash: genesis_config.hash(),
                ledger_path: ledger_path.to_path_buf(),
                validator_exit: config.validator_exit.clone(),
                exit: exit.clone(),
                override_health_check: rpc_override_health_check.clone(),
                optimistically_confirmed_bank: optimistically_confirmed_bank.clone(),
                send_transaction_service_config: config.send_transaction_service_config.clone(),
                max_slots: max_slots.clone(),
                leader_schedule_cache: leader_schedule_cache.clone(),
                max_complete_transaction_status_slot: max_complete_transaction_status_slot.clone(),
                prioritization_fee_cache: prioritization_fee_cache.clone(),
                rpc_tpu_client_args,
            };
            let json_rpc_service =
                JsonRpcService::new_with_config(rpc_svc_config).map_err(ValidatorError::Other)?;
            let rpc_subscriptions = Arc::new(RpcSubscriptions::new_with_config(
                exit.clone(),
                max_complete_transaction_status_slot,
                blockstore.clone(),
                bank_forks.clone(),
                block_commitment_cache.clone(),
                optimistically_confirmed_bank.clone(),
                &config.pubsub_config,
                None,
            ));
            let pubsub_service = if !config.rpc_config.full_api {
                None
            } else {
                let (trigger, pubsub_service) = PubSubService::new(
                    config.pubsub_config.clone(),
                    &rpc_subscriptions,
                    rpc_pubsub_addr,
                );
                config
                    .validator_exit
                    .write()
                    .unwrap()
                    .register_exit(Box::new(move || trigger.cancel()));

                Some(pubsub_service)
            };

            let (completed_data_sets_sender, completed_data_sets_service) =
                if !config.rpc_config.full_api {
                    (None, None)
                } else {
                    let (completed_data_sets_sender, completed_data_sets_receiver) =
                        bounded(MAX_COMPLETED_DATA_SETS_IN_CHANNEL);
                    let completed_data_sets_service = CompletedDataSetsService::new(
                        completed_data_sets_receiver,
                        blockstore.clone(),
                        rpc_subscriptions.clone(),
                        exit.clone(),
                        max_slots.clone(),
                    );
                    (
                        Some(completed_data_sets_sender),
                        Some(completed_data_sets_service),
                    )
                };

            let rpc_completed_slots_service =
                if config.rpc_config.full_api || geyser_plugin_service.is_some() {
                    let (completed_slots_sender, completed_slots_receiver) =
                        bounded(MAX_COMPLETED_SLOTS_IN_CHANNEL);
                    blockstore.add_completed_slots_signal(completed_slots_sender);

                    Some(RpcCompletedSlotsService::spawn(
                        completed_slots_receiver,
                        rpc_subscriptions.clone(),
                        slot_status_notifier.clone(),
                        exit.clone(),
                    ))
                } else {
                    None
                };

            let dependency_tracker = transaction_status_sender
                .is_some()
                .then_some(dependency_tracker);
            let optimistically_confirmed_bank_tracker =
                Some(OptimisticallyConfirmedBankTracker::new(
                    bank_notification_receiver,
                    exit.clone(),
                    bank_forks.clone(),
                    optimistically_confirmed_bank,
                    rpc_subscriptions.clone(),
                    confirmed_bank_subscribers,
                    prioritization_fee_cache.clone(),
                    dependency_tracker.clone(),
                ));
            let bank_notification_sender_config = Some(BankNotificationSenderConfig {
                sender: bank_notification_sender,
                should_send_parents: geyser_plugin_service.is_some(),
                dependency_tracker,
            });
            (
                Some(json_rpc_service),
                Some(rpc_subscriptions),
                pubsub_service,
                completed_data_sets_sender,
                completed_data_sets_service,
                rpc_completed_slots_service,
                optimistically_confirmed_bank_tracker,
                bank_notification_sender_config,
            )
        } else {
            (None, None, None, None, None, None, None, None)
        };

        let ip_echo_server = match node.sockets.ip_echo {
            None => None,
            Some(tcp_listener) => Some(solana_net_utils::ip_echo_server(
                tcp_listener,
                config.ip_echo_server_threads,
                Some(node.info.shred_version()),
            )),
        };

        let (stats_reporter_sender, stats_reporter_receiver) = unbounded();

        let stats_reporter_service =
            StatsReporterService::new(stats_reporter_receiver, exit.clone());

        let gossip_service = GossipService::new(
            &cluster_info,
            Some(bank_forks.clone()),
            node.sockets.gossip.clone(),
            config.gossip_validators.clone(),
            should_check_duplicate_instance,
            Some(stats_reporter_sender.clone()),
            exit.clone(),
        );
        let serve_repair = config.repair_handler_type.create_serve_repair(
            blockstore.clone(),
            cluster_info.clone(),
            bank_forks.read().unwrap().sharable_banks(),
            config.repair_whitelist.clone(),
        );
        let (repair_request_quic_sender, repair_request_quic_receiver) = unbounded();
        let (repair_response_quic_sender, repair_response_quic_receiver) = unbounded();
        let (ancestor_hashes_response_quic_sender, ancestor_hashes_response_quic_receiver) =
            unbounded();

        let waited_for_supermajority = wait_for_supermajority(
            config,
            Some(&mut process_blockstore),
            &bank_forks,
            &cluster_info,
            rpc_override_health_check,
            &start_progress,
        )?;

        let blockstore_metric_report_service =
            BlockstoreMetricReportService::new(blockstore.clone(), exit.clone());

        let wait_for_vote_to_start_leader =
            !waited_for_supermajority && !config.no_wait_for_vote_to_start_leader;

        let poh_service = PohService::new(
            poh_recorder.clone(),
            &genesis_config.poh_config,
            exit.clone(),
            bank_forks.read().unwrap().root_bank().ticks_per_slot(),
            config.poh_pinned_cpu_core,
            config.poh_hashes_per_batch,
            record_receiver,
            poh_service_message_receiver,
        );
        assert_eq!(
            blockstore.get_new_shred_signals_len(),
            1,
            "New shred signal for the TVU should be the same as the clear bank signal."
        );

        let vote_tracker = Arc::<VoteTracker>::default();

        let (retransmit_slots_sender, retransmit_slots_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = unbounded();
        let (duplicate_confirmed_slot_sender, duplicate_confirmed_slots_receiver) = unbounded();

        let entry_notification_sender = entry_notifier_service
            .as_ref()
            .map(|service| service.sender_cloned());

        let turbine_quic_endpoint_runtime = (current_runtime_handle.is_err()
            && genesis_config.cluster_type != ClusterType::MainnetBeta)
            .then(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("solTurbineQuic")
                    .build()
                    .unwrap()
            });
        let (turbine_quic_endpoint_sender, turbine_quic_endpoint_receiver) = unbounded();
        let (
            turbine_quic_endpoint,
            turbine_quic_endpoint_sender,
            turbine_quic_endpoint_join_handle,
        ) = if genesis_config.cluster_type == ClusterType::MainnetBeta {
            let (sender, _receiver) = tokio::sync::mpsc::channel(1);
            (None, sender, None)
        } else {
            solana_turbine::quic_endpoint::new_quic_endpoint(
                turbine_quic_endpoint_runtime
                    .as_ref()
                    .map(TokioRuntime::handle)
                    .unwrap_or_else(|| current_runtime_handle.as_ref().unwrap()),
                &identity_keypair,
                node.sockets.tvu_quic,
                turbine_quic_endpoint_sender,
                bank_forks.clone(),
            )
            .map(|(endpoint, sender, join_handle)| (Some(endpoint), sender, Some(join_handle)))
            .unwrap()
        };

        // Repair quic endpoint.
        let repair_quic_endpoints_runtime = (current_runtime_handle.is_err()
            && genesis_config.cluster_type != ClusterType::MainnetBeta)
            .then(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("solRepairQuic")
                    .build()
                    .unwrap()
            });
        let (repair_quic_endpoints, repair_quic_async_senders, repair_quic_endpoints_join_handle) =
            if genesis_config.cluster_type == ClusterType::MainnetBeta {
                (None, RepairQuicAsyncSenders::new_dummy(), None)
            } else {
                let repair_quic_sockets = RepairQuicSockets {
                    repair_server_quic_socket: node.sockets.serve_repair_quic,
                    repair_client_quic_socket: node.sockets.repair_quic,
                    ancestor_hashes_quic_socket: node.sockets.ancestor_hashes_requests_quic,
                };
                let repair_quic_senders = RepairQuicSenders {
                    repair_request_quic_sender: repair_request_quic_sender.clone(),
                    repair_response_quic_sender,
                    ancestor_hashes_response_quic_sender,
                };
                repair::quic_endpoint::new_quic_endpoints(
                    repair_quic_endpoints_runtime
                        .as_ref()
                        .map(TokioRuntime::handle)
                        .unwrap_or_else(|| current_runtime_handle.as_ref().unwrap()),
                    &identity_keypair,
                    repair_quic_sockets,
                    repair_quic_senders,
                    bank_forks.clone(),
                )
                .map(|(endpoints, senders, join_handle)| {
                    (Some(endpoints), senders, Some(join_handle))
                })
                .unwrap()
            };
        let serve_repair_service = ServeRepairService::new(
            serve_repair,
            // Incoming UDP repair requests are adapted into RemoteRequest
            // and also sent through the same channel.
            repair_request_quic_sender,
            repair_request_quic_receiver,
            repair_quic_async_senders.repair_response_quic_sender,
            node.sockets.serve_repair,
            socket_addr_space,
            stats_reporter_sender,
            exit.clone(),
        );

        let in_wen_restart = config.wen_restart_proto_path.is_some() && !waited_for_supermajority;
        let wen_restart_repair_slots = if in_wen_restart {
            Some(Arc::new(RwLock::new(Vec::new())))
        } else {
            None
        };
        let tower = match process_blockstore.process_to_create_tower() {
            Ok(tower) => {
                info!("Tower state: {tower:?}");
                tower
            }
            Err(e) => {
                warn!("Unable to retrieve tower: {e:?} creating default tower....");
                Tower::default()
            }
        };
        let last_vote = tower.last_vote();

        let outstanding_repair_requests =
            Arc::<RwLock<repair::repair_service::OutstandingShredRepairs>>::default();
        let root_bank = bank_forks.read().unwrap().root_bank();
        let cluster_slots = Arc::new({
            crate::cluster_slots_service::cluster_slots::ClusterSlots::new(
                &root_bank,
                &cluster_info,
            )
        });

        let (xdp_retransmitter, xdp_sender) = if let Some(xdp_config) =
            config.retransmit_xdp.clone()
        {
            let src_port = node.sockets.retransmit_sockets[0]
                .local_addr()
                .expect("failed to get local address")
                .port();
            let src_ip = match node.bind_ip_addrs.active() {
                IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
                IpAddr::V4(_unspecified) => xdp_config
                    .interface
                    .as_ref()
                    .and_then(|iface| master_ip_if_bonded(iface)),
                _ => panic!("IPv6 not supported"),
            };
            let (rtx, sender) = XdpRetransmitter::new(xdp_config, src_port, src_ip, exit.clone())
                .expect("failed to create xdp retransmitter");
            (Some(rtx), Some(sender))
        } else {
            (None, None)
        };

        // disable all2all tests if not allowed for a given cluster type
        let alpenglow_socket = if genesis_config.cluster_type == ClusterType::Testnet
            || genesis_config.cluster_type == ClusterType::Development
        {
            node.sockets.alpenglow
        } else {
            None
        };

        let tvu = Tvu::new(
            vote_account,
            authorized_voter_keypairs,
            &bank_forks,
            &cluster_info,
            TvuSockets {
                repair: node.sockets.repair.try_clone().unwrap(),
                retransmit: node.sockets.retransmit_sockets,
                fetch: node.sockets.tvu,
                ancestor_hashes_requests: node.sockets.ancestor_hashes_requests,
                alpenglow: alpenglow_socket,
            },
            blockstore.clone(),
            ledger_signal_receiver,
            rpc_subscriptions.clone(),
            &poh_recorder,
            poh_controller,
            tower,
            config.tower_storage.clone(),
            &leader_schedule_cache,
            exit.clone(),
            block_commitment_cache,
            config.turbine_disabled.clone(),
            transaction_status_sender.clone(),
            entry_notification_sender.clone(),
            vote_tracker.clone(),
            retransmit_slots_sender,
            gossip_verified_vote_hash_receiver,
            verified_vote_receiver,
            replay_vote_sender.clone(),
            completed_data_sets_sender,
            bank_notification_sender.clone(),
            duplicate_confirmed_slots_receiver,
            TvuConfig {
                max_ledger_shreds: config.max_ledger_shreds,
                shred_version: node.info.shred_version(),
                repair_validators: config.repair_validators.clone(),
                repair_whitelist: config.repair_whitelist.clone(),
                wait_for_vote_to_start_leader,
                replay_forks_threads: config.replay_forks_threads,
                replay_transactions_threads: config.replay_transactions_threads,
                shred_sigverify_threads: config.tvu_shred_sigverify_threads,
                xdp_sender: xdp_sender.clone(),
            },
            &max_slots,
            block_metadata_notifier,
            config.wait_to_vote_slot,
            Some(snapshot_controller.clone()),
            config.runtime_config.log_messages_bytes_limit,
            prioritization_fee_cache.clone(),
            banking_tracer.clone(),
            turbine_quic_endpoint_sender.clone(),
            turbine_quic_endpoint_receiver,
            repair_response_quic_receiver,
            repair_quic_async_senders.repair_request_quic_sender,
            repair_quic_async_senders.ancestor_hashes_request_quic_sender,
            ancestor_hashes_response_quic_receiver,
            outstanding_repair_requests.clone(),
            cluster_slots.clone(),
            wen_restart_repair_slots.clone(),
            slot_status_notifier,
            vote_connection_cache,
        )
        .map_err(ValidatorError::Other)?;

        if in_wen_restart {
            info!("Waiting for wen_restart to finish");
            wait_for_wen_restart(WenRestartConfig {
                wen_restart_path: config.wen_restart_proto_path.clone().unwrap(),
                wen_restart_coordinator: config.wen_restart_coordinator.unwrap(),
                last_vote,
                blockstore: blockstore.clone(),
                cluster_info: cluster_info.clone(),
                bank_forks: bank_forks.clone(),
                wen_restart_repair_slots: wen_restart_repair_slots.clone(),
                wait_for_supermajority_threshold_percent:
                    WAIT_FOR_WEN_RESTART_SUPERMAJORITY_THRESHOLD_PERCENT,
                snapshot_controller: Some(snapshot_controller.clone()),
                abs_status: accounts_background_service.status().clone(),
                genesis_config_hash: genesis_config.hash(),
                exit: exit.clone(),
            })?;
            return Err(ValidatorError::WenRestartFinished.into());
        }

        let key_notifiers = Arc::new(RwLock::new(KeyUpdaters::default()));
        let forwarding_tpu_client = {
            let runtime_handle = tpu_client_next_runtime
                .as_ref()
                .map(TokioRuntime::handle)
                .unwrap_or_else(|| current_runtime_handle.as_ref().unwrap());
            ForwardingClientOption::TpuClientNext((
                Arc::as_ref(&identity_keypair),
                tpu_transactions_forwards_client_sockets.take().unwrap(),
                runtime_handle.clone(),
                cancel.clone(),
                node_multihoming.clone(),
            ))
        };
        let (banking_control_sender, banking_control_reciever) = mpsc::channel(1);
        let tpu = Tpu::new_with_client(
            &cluster_info,
            &poh_recorder,
            transaction_recorder,
            entry_receiver,
            retransmit_slots_receiver,
            TpuSockets {
                transactions: node.sockets.tpu,
                transaction_forwards: node.sockets.tpu_forwards,
                vote: node.sockets.tpu_vote,
                broadcast: node.sockets.broadcast,
                transactions_quic: node.sockets.tpu_quic,
                transactions_forwards_quic: node.sockets.tpu_forwards_quic,
                vote_quic: node.sockets.tpu_vote_quic,
                vote_forwarding_client: node.sockets.tpu_vote_forwarding_client,
                vortexor_receivers: node.sockets.vortexor_receivers,
            },
            rpc_subscriptions.clone(),
            transaction_status_sender,
            entry_notification_sender,
            blockstore.clone(),
            &config.broadcast_stage_type,
            xdp_sender,
            exit.clone(),
            node.info.shred_version(),
            vote_tracker,
            bank_forks.clone(),
            verified_vote_sender,
            gossip_verified_vote_hash_sender,
            replay_vote_receiver,
            replay_vote_sender,
            bank_notification_sender,
            duplicate_confirmed_slot_sender,
            forwarding_tpu_client,
            turbine_quic_endpoint_sender,
            &identity_keypair,
            config.runtime_config.log_messages_bytes_limit,
            &staked_nodes,
            config.staked_nodes_overrides.clone(),
            banking_tracer_channels,
            tracer_thread,
            tpu_enable_udp,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
            prioritization_fee_cache,
            config.block_production_method.clone(),
            config.block_production_num_workers,
            config.block_production_scheduler_config.clone(),
            config.enable_block_production_forwarding,
            config.generator_config.clone(),
            key_notifiers.clone(),
            banking_control_reciever,
            config.enable_scheduler_bindings.then(|| {
                (
                    ledger_path.join("scheduler_bindings.ipc"),
                    banking_control_sender.clone(),
                )
            }),
            cancel,
        );

        datapoint_info!(
            "validator-new",
            ("id", id.to_string(), String),
            ("version", solana_version::version!(), String),
            ("cluster_type", genesis_config.cluster_type as u32, i64),
            ("elapsed_ms", start_time.elapsed().as_millis() as i64, i64),
            ("waited_for_supermajority", waited_for_supermajority, bool),
            ("shred_version", shred_version as i64, i64),
        );

        *start_progress.write().unwrap() = ValidatorStartProgress::Running;
        if let Some(json_rpc_service) = &json_rpc_service {
            key_notifiers.write().unwrap().add(
                KeyUpdaterType::RpcService,
                json_rpc_service.get_client_key_updater(),
            );
        }

        *admin_rpc_service_post_init.write().unwrap() = Some(AdminRpcRequestMetadataPostInit {
            bank_forks: bank_forks.clone(),
            cluster_info: cluster_info.clone(),
            vote_account: *vote_account,
            repair_whitelist: config.repair_whitelist.clone(),
            notifies: key_notifiers,
            repair_socket: Arc::new(node.sockets.repair),
            outstanding_repair_requests,
            cluster_slots,
            node: Some(node_multihoming),
            banking_control_sender,
        });

        Ok(Self {
            logfile: config.logfile.clone(),
            exit,
            stats_reporter_service,
            gossip_service,
            serve_repair_service,
            json_rpc_service,
            pubsub_service,
            rpc_completed_slots_service,
            optimistically_confirmed_bank_tracker,
            transaction_status_service,
            entry_notifier_service,
            system_monitor_service,
            sample_performance_service,
            snapshot_packager_service,
            completed_data_sets_service,
            tpu,
            tvu,
            poh_service,
            poh_recorder,
            ip_echo_server,
            validator_exit: config.validator_exit.clone(),
            cluster_info,
            bank_forks,
            blockstore,
            geyser_plugin_service,
            blockstore_metric_report_service,
            accounts_background_service,
            turbine_quic_endpoint,
            turbine_quic_endpoint_runtime,
            turbine_quic_endpoint_join_handle,
            repair_quic_endpoints,
            repair_quic_endpoints_runtime,
            repair_quic_endpoints_join_handle,
            xdp_retransmitter,
            _tpu_client_next_runtime: tpu_client_next_runtime,
        })
    }

    /// Register and listen for signals that the validator will act on. Also,
    /// monitor the validator's exit flag incase a shutdown has been initated
    /// by one of the validator threads
    pub fn listen_for_signals(&self) -> Result<()> {
        // Reopen the logfile when the SIGUSR1 signal is received; this provides
        // a hook for working with logrotate
        let sigusr1_flag = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        {
            if self.logfile.is_some() {
                signal_hook::flag::register(libc::SIGUSR1, sigusr1_flag.clone())?;
            }
        }

        info!("Validator::listen_for_signals() has started");
        loop {
            if self.exit.load(Ordering::Relaxed) {
                break;
            }

            if sigusr1_flag.load(Ordering::Relaxed) {
                #[cfg(unix)]
                {
                    if let Some(logfile) = self.logfile.as_ref() {
                        info!("Received SIGUSR1, reopening {}", logfile.display());
                        agave_logger::redirect_stderr(logfile);
                        // Reset the flag to `false` to allow detection of the
                        // signal again and to avoid hitting this case every
                        // iteration
                        sigusr1_flag.store(false, Ordering::Relaxed);
                    }
                }
                #[cfg(not(unix))]
                {
                    unreachable!("The SIGUSR1 signal is only handled on unix systems");
                }
            }

            // One second is a reasonable response time for these signals to
            // avoid this thread from being overly active
            thread::sleep(Duration::from_secs(1));
        }
        info!("Validator::listen_for_signals() has stopped");

        Ok(())
    }

    // Used for notifying many nodes in parallel to exit
    pub fn exit(&mut self) {
        self.validator_exit.write().unwrap().exit();

        // drop all signals in blockstore
        self.blockstore.drop_signal();
    }

    pub fn close(mut self) {
        self.exit();
        self.join();
    }

    fn print_node_info(node: &Node) {
        info!("{:?}", node.info);
        info!(
            "local gossip address: {}",
            node.sockets.gossip[0].local_addr().unwrap()
        );
        info!(
            "local broadcast address: {}",
            node.sockets
                .broadcast
                .first()
                .unwrap()
                .local_addr()
                .unwrap()
        );
        info!(
            "local repair address: {}",
            node.sockets.repair.local_addr().unwrap()
        );
        info!(
            "local retransmit address: {}",
            node.sockets.retransmit_sockets[0].local_addr().unwrap()
        );
    }

    pub fn join(self) {
        drop(self.bank_forks);
        drop(self.cluster_info);

        self.poh_service.join().expect("poh_service");
        drop(self.poh_recorder);

        if let Some(json_rpc_service) = self.json_rpc_service {
            json_rpc_service.join().expect("rpc_service");
        }

        if let Some(pubsub_service) = self.pubsub_service {
            pubsub_service.join().expect("pubsub_service");
        }

        if let Some(rpc_completed_slots_service) = self.rpc_completed_slots_service {
            rpc_completed_slots_service
                .join()
                .expect("rpc_completed_slots_service");
        }

        if let Some(optimistically_confirmed_bank_tracker) =
            self.optimistically_confirmed_bank_tracker
        {
            optimistically_confirmed_bank_tracker
                .join()
                .expect("optimistically_confirmed_bank_tracker");
        }

        if let Some(transaction_status_service) = self.transaction_status_service {
            transaction_status_service
                .join()
                .expect("transaction_status_service");
        }

        if let Some(system_monitor_service) = self.system_monitor_service {
            system_monitor_service
                .join()
                .expect("system_monitor_service");
        }

        if let Some(sample_performance_service) = self.sample_performance_service {
            sample_performance_service
                .join()
                .expect("sample_performance_service");
        }

        if let Some(entry_notifier_service) = self.entry_notifier_service {
            entry_notifier_service
                .join()
                .expect("entry_notifier_service");
        }

        if let Some(s) = self.snapshot_packager_service {
            s.join().expect("snapshot_packager_service");
        }

        self.gossip_service.join().expect("gossip_service");
        self.repair_quic_endpoints
            .iter()
            .flatten()
            .for_each(repair::quic_endpoint::close_quic_endpoint);
        self.serve_repair_service
            .join()
            .expect("serve_repair_service");
        if let Some(repair_quic_endpoints_join_handle) = self.repair_quic_endpoints_join_handle {
            self.repair_quic_endpoints_runtime
                .map(|runtime| runtime.block_on(repair_quic_endpoints_join_handle))
                .transpose()
                .unwrap();
        }
        self.stats_reporter_service
            .join()
            .expect("stats_reporter_service");
        self.blockstore_metric_report_service
            .join()
            .expect("ledger_metric_report_service");
        self.accounts_background_service
            .join()
            .expect("accounts_background_service");
        if let Some(turbine_quic_endpoint) = &self.turbine_quic_endpoint {
            solana_turbine::quic_endpoint::close_quic_endpoint(turbine_quic_endpoint);
        }
        if let Some(xdp_retransmitter) = self.xdp_retransmitter {
            xdp_retransmitter.join().expect("xdp_retransmitter");
        }
        self.tpu.join().expect("tpu");
        self.tvu.join().expect("tvu");
        if let Some(turbine_quic_endpoint_join_handle) = self.turbine_quic_endpoint_join_handle {
            self.turbine_quic_endpoint_runtime
                .map(|runtime| runtime.block_on(turbine_quic_endpoint_join_handle))
                .transpose()
                .unwrap();
        }
        if let Some(completed_data_sets_service) = self.completed_data_sets_service {
            completed_data_sets_service
                .join()
                .expect("completed_data_sets_service");
        }
        if let Some(ip_echo_server) = self.ip_echo_server {
            ip_echo_server.shutdown_background();
        }

        if let Some(geyser_plugin_service) = self.geyser_plugin_service {
            geyser_plugin_service.join().expect("geyser_plugin_service");
        }
    }
}

fn active_vote_account_exists_in_bank(bank: &Bank, vote_account: &Pubkey) -> bool {
    if let Some(account) = &bank.get_account(vote_account) {
        if let Ok(vote_state) = VoteStateV4::deserialize(account.data(), vote_account) {
            return !vote_state.votes.is_empty();
        }
    }
    false
}

fn check_poh_speed(bank: &Bank, maybe_hash_samples: Option<u64>) -> Result<(), ValidatorError> {
    let Some(hashes_per_tick) = bank.hashes_per_tick() else {
        warn!("Unable to read hashes per tick from Bank, skipping PoH speed check");
        return Ok(());
    };

    let ticks_per_slot = bank.ticks_per_slot();
    let hashes_per_slot = hashes_per_tick * ticks_per_slot;
    let hash_samples = maybe_hash_samples.unwrap_or(hashes_per_slot);

    let hash_time = compute_hash_time(hash_samples);
    let my_hashes_per_second = (hash_samples as f64 / hash_time.as_secs_f64()) as u64;

    let target_slot_duration = Duration::from_nanos(bank.ns_per_slot as u64);
    let target_hashes_per_second =
        (hashes_per_slot as f64 / target_slot_duration.as_secs_f64()) as u64;

    info!(
        "PoH speed check: computed hashes per second {my_hashes_per_second}, target hashes per \
         second {target_hashes_per_second}"
    );
    if my_hashes_per_second < target_hashes_per_second {
        return Err(ValidatorError::PohTooSlow {
            mine: my_hashes_per_second,
            target: target_hashes_per_second,
        });
    }

    Ok(())
}

fn maybe_cluster_restart_with_hard_fork(config: &ValidatorConfig, root_slot: Slot) -> Option<Slot> {
    // detect cluster restart (hard fork) indirectly via wait_for_supermajority...
    if let Some(wait_slot_for_supermajority) = config.wait_for_supermajority {
        if wait_slot_for_supermajority == root_slot {
            return Some(wait_slot_for_supermajority);
        }
    }

    None
}

fn post_process_restored_tower(
    restored_tower: crate::consensus::Result<Tower>,
    validator_identity: &Pubkey,
    vote_account: &Pubkey,
    config: &ValidatorConfig,
    bank_forks: &BankForks,
) -> Result<Tower, String> {
    let mut should_require_tower = config.require_tower;

    let restored_tower = restored_tower.and_then(|tower| {
        let root_bank = bank_forks.root_bank();
        let slot_history = root_bank.get_slot_history();
        // make sure tower isn't corrupted first before the following hard fork check
        let tower = tower.adjust_lockouts_after_replay(root_bank.slot(), &slot_history);

        if let Some(hard_fork_restart_slot) =
            maybe_cluster_restart_with_hard_fork(config, root_bank.slot())
        {
            // intentionally fail to restore tower; we're supposedly in a new hard fork; past
            // out-of-chain vote state doesn't make sense at all
            // what if --wait-for-supermajority again if the validator restarted?
            let message =
                format!("Hard fork is detected; discarding tower restoration result: {tower:?}");
            datapoint_error!("tower_error", ("error", message, String),);
            error!("{message}");

            // unconditionally relax tower requirement so that we can always restore tower
            // from root bank.
            should_require_tower = false;
            return Err(crate::consensus::TowerError::HardFork(
                hard_fork_restart_slot,
            ));
        }

        if let Some(warp_slot) = config.warp_slot {
            // unconditionally relax tower requirement so that we can always restore tower
            // from root bank after the warp
            should_require_tower = false;
            return Err(crate::consensus::TowerError::HardFork(warp_slot));
        }

        tower
    });

    let restored_tower = match restored_tower {
        Ok(tower) => tower,
        Err(err) => {
            let voting_has_been_active =
                active_vote_account_exists_in_bank(&bank_forks.working_bank(), vote_account);
            if !err.is_file_missing() {
                datapoint_error!(
                    "tower_error",
                    ("error", format!("Unable to restore tower: {err}"), String),
                );
            }
            if should_require_tower && voting_has_been_active {
                return Err(format!(
                    "Requested mandatory tower restore failed: {err}. And there is an existing \
                     vote_account containing actual votes. Aborting due to possible conflicting \
                     duplicate votes"
                ));
            }
            if err.is_file_missing() && !voting_has_been_active {
                // Currently, don't protect against spoofed snapshots with no tower at all
                info!(
                    "Ignoring expected failed tower restore because this is the initial validator \
                     start with the vote account..."
                );
            } else {
                error!(
                    "Rebuilding a new tower from the latest vote account due to failed tower \
                     restore: {err}"
                );
            }

            Tower::new_from_bankforks(bank_forks, validator_identity, vote_account)
        }
    };

    Ok(restored_tower)
}

fn load_genesis(
    config: &ValidatorConfig,
    ledger_path: &Path,
) -> Result<GenesisConfig, ValidatorError> {
    let genesis_config = open_genesis_config(ledger_path, config.max_genesis_archive_unpacked_size)
        .map_err(ValidatorError::OpenGenesisConfig)?;

    // This needs to be limited otherwise the state in the VoteAccount data
    // grows too large
    let leader_schedule_slot_offset = genesis_config.epoch_schedule.leader_schedule_slot_offset;
    let slots_per_epoch = genesis_config.epoch_schedule.slots_per_epoch;
    let leader_epoch_offset = leader_schedule_slot_offset.div_ceil(slots_per_epoch);
    assert!(leader_epoch_offset <= MAX_LEADER_SCHEDULE_EPOCH_OFFSET);

    let genesis_hash = genesis_config.hash();
    info!("genesis hash: {genesis_hash}");

    if let Some(expected_genesis_hash) = config.expected_genesis_hash {
        if genesis_hash != expected_genesis_hash {
            return Err(ValidatorError::GenesisHashMismatch(
                genesis_hash,
                expected_genesis_hash,
            ));
        }
    }

    Ok(genesis_config)
}

#[allow(clippy::type_complexity)]
fn load_blockstore(
    config: &ValidatorConfig,
    ledger_path: &Path,
    genesis_config: &GenesisConfig,
    exit: Arc<AtomicBool>,
    start_progress: &Arc<RwLock<ValidatorStartProgress>>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    transaction_notifier: Option<TransactionNotifierArc>,
    entry_notifier: Option<EntryNotifierArc>,
    dependency_tracker: Option<Arc<DependencyTracker>>,
) -> Result<
    (
        Arc<RwLock<BankForks>>,
        Arc<Blockstore>,
        Slot,
        Receiver<bool>,
        LeaderScheduleCache,
        Option<StartingSnapshotHashes>,
        TransactionHistoryServices,
        blockstore_processor::ProcessOptions,
        BlockstoreRootScan,
        DroppedSlotsReceiver,
        Option<EntryNotifierService>,
    ),
    String,
> {
    info!("loading ledger from {ledger_path:?}...");
    *start_progress.write().unwrap() = ValidatorStartProgress::LoadingLedger;

    let blockstore = Blockstore::open_with_options(ledger_path, config.blockstore_options.clone())
        .map_err(|err| format!("Failed to open Blockstore: {err:?}"))?;

    let (ledger_signal_sender, ledger_signal_receiver) = bounded(MAX_REPLAY_WAKE_UP_SIGNALS);
    blockstore.add_new_shred_signal(ledger_signal_sender);

    // following boot sequence (esp BankForks) could set root. so stash the original value
    // of blockstore root away here as soon as possible.
    let original_blockstore_root = blockstore.max_root();

    let blockstore = Arc::new(blockstore);
    let blockstore_root_scan = BlockstoreRootScan::new(config, blockstore.clone(), exit.clone());
    let halt_at_slot = blockstore.highest_slot().unwrap_or(None);

    let process_options = blockstore_processor::ProcessOptions {
        run_verification: config.run_verification,
        halt_at_slot,
        new_hard_forks: config.new_hard_forks.clone(),
        debug_keys: config.debug_keys.clone(),
        accounts_db_config: config.accounts_db_config.clone(),
        accounts_db_skip_shrink: config.accounts_db_skip_shrink,
        accounts_db_force_initial_clean: config.accounts_db_force_initial_clean,
        runtime_config: config.runtime_config.clone(),
        use_snapshot_archives_at_startup: config.use_snapshot_archives_at_startup,
        ..blockstore_processor::ProcessOptions::default()
    };

    let enable_rpc_transaction_history =
        config.rpc_addrs.is_some() && config.rpc_config.enable_rpc_transaction_history;
    let is_plugin_transaction_history_required = transaction_notifier.as_ref().is_some();
    let transaction_history_services =
        if enable_rpc_transaction_history || is_plugin_transaction_history_required {
            initialize_rpc_transaction_history_services(
                blockstore.clone(),
                exit.clone(),
                enable_rpc_transaction_history,
                config.rpc_config.enable_extended_tx_metadata_storage,
                transaction_notifier,
                dependency_tracker,
            )
        } else {
            TransactionHistoryServices::default()
        };

    let entry_notifier_service = entry_notifier
        .map(|entry_notifier| EntryNotifierService::new(entry_notifier, exit.clone()));

    let (bank_forks, mut leader_schedule_cache, starting_snapshot_hashes) =
        bank_forks_utils::load_bank_forks(
            genesis_config,
            &blockstore,
            config.account_paths.clone(),
            &config.snapshot_config,
            &process_options,
            transaction_history_services
                .transaction_status_sender
                .as_ref(),
            entry_notifier_service
                .as_ref()
                .map(|service| service.sender()),
            accounts_update_notifier,
            exit,
        )
        .map_err(|err| err.to_string())?;

    // Before replay starts, set the callbacks in each of the banks in BankForks so that
    // all dropped banks come through the `pruned_banks_receiver` channel. This way all bank
    // drop behavior can be safely synchronized with any other ongoing accounts activity like
    // cache flush, clean, shrink, as long as the same thread performing those activities also
    // is processing the dropped banks from the `pruned_banks_receiver` channel.
    let pruned_banks_receiver =
        AccountsBackgroundService::setup_bank_drop_callback(bank_forks.clone());

    leader_schedule_cache.set_fixed_leader_schedule(config.fixed_leader_schedule.clone());

    Ok((
        bank_forks,
        blockstore,
        original_blockstore_root,
        ledger_signal_receiver,
        leader_schedule_cache,
        starting_snapshot_hashes,
        transaction_history_services,
        process_options,
        blockstore_root_scan,
        pruned_banks_receiver,
        entry_notifier_service,
    ))
}

pub struct ProcessBlockStore<'a> {
    id: &'a Pubkey,
    vote_account: &'a Pubkey,
    start_progress: &'a Arc<RwLock<ValidatorStartProgress>>,
    blockstore: &'a Blockstore,
    original_blockstore_root: Slot,
    bank_forks: &'a Arc<RwLock<BankForks>>,
    leader_schedule_cache: &'a LeaderScheduleCache,
    process_options: &'a blockstore_processor::ProcessOptions,
    transaction_status_sender: Option<&'a TransactionStatusSender>,
    entry_notification_sender: Option<&'a EntryNotifierSender>,
    blockstore_root_scan: Option<BlockstoreRootScan>,
    snapshot_controller: &'a SnapshotController,
    config: &'a ValidatorConfig,
    tower: Option<Tower>,
}

impl<'a> ProcessBlockStore<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: &'a Pubkey,
        vote_account: &'a Pubkey,
        start_progress: &'a Arc<RwLock<ValidatorStartProgress>>,
        blockstore: &'a Blockstore,
        original_blockstore_root: Slot,
        bank_forks: &'a Arc<RwLock<BankForks>>,
        leader_schedule_cache: &'a LeaderScheduleCache,
        process_options: &'a blockstore_processor::ProcessOptions,
        transaction_status_sender: Option<&'a TransactionStatusSender>,
        entry_notification_sender: Option<&'a EntryNotifierSender>,
        blockstore_root_scan: BlockstoreRootScan,
        snapshot_controller: &'a SnapshotController,
        config: &'a ValidatorConfig,
    ) -> Self {
        Self {
            id,
            vote_account,
            start_progress,
            blockstore,
            original_blockstore_root,
            bank_forks,
            leader_schedule_cache,
            process_options,
            transaction_status_sender,
            entry_notification_sender,
            blockstore_root_scan: Some(blockstore_root_scan),
            snapshot_controller,
            config,
            tower: None,
        }
    }

    pub(crate) fn process(&mut self) -> Result<(), String> {
        if self.tower.is_none() {
            let previous_start_process = *self.start_progress.read().unwrap();
            *self.start_progress.write().unwrap() = ValidatorStartProgress::LoadingLedger;

            let exit = Arc::new(AtomicBool::new(false));
            if let Ok(Some(max_slot)) = self.blockstore.highest_slot() {
                let bank_forks = self.bank_forks.clone();
                let exit = exit.clone();
                let start_progress = self.start_progress.clone();

                let _ = Builder::new()
                    .name("solRptLdgrStat".to_string())
                    .spawn(move || {
                        while !exit.load(Ordering::Relaxed) {
                            let slot = bank_forks.read().unwrap().working_bank().slot();
                            *start_progress.write().unwrap() =
                                ValidatorStartProgress::ProcessingLedger { slot, max_slot };
                            thread::sleep(Duration::from_secs(2));
                        }
                    })
                    .unwrap();
            }
            blockstore_processor::process_blockstore_from_root(
                self.blockstore,
                self.bank_forks,
                self.leader_schedule_cache,
                self.process_options,
                self.transaction_status_sender,
                self.entry_notification_sender,
                Some(self.snapshot_controller),
            )
            .map_err(|err| {
                exit.store(true, Ordering::Relaxed);
                format!("Failed to load ledger: {err:?}")
            })?;
            exit.store(true, Ordering::Relaxed);

            if let Some(blockstore_root_scan) = self.blockstore_root_scan.take() {
                blockstore_root_scan.join();
            }

            self.tower = Some({
                let restored_tower = Tower::restore(self.config.tower_storage.as_ref(), self.id);
                if let Ok(tower) = &restored_tower {
                    // reconciliation attempt 1 of 2 with tower
                    reconcile_blockstore_roots_with_external_source(
                        ExternalRootSource::Tower(tower.root()),
                        self.blockstore,
                        &mut self.original_blockstore_root,
                    )
                    .map_err(|err| format!("Failed to reconcile blockstore with tower: {err:?}"))?;
                }

                post_process_restored_tower(
                    restored_tower,
                    self.id,
                    self.vote_account,
                    self.config,
                    &self.bank_forks.read().unwrap(),
                )?
            });

            if let Some(hard_fork_restart_slot) = maybe_cluster_restart_with_hard_fork(
                self.config,
                self.bank_forks.read().unwrap().root(),
            ) {
                // reconciliation attempt 2 of 2 with hard fork
                // this should be #2 because hard fork root > tower root in almost all cases
                reconcile_blockstore_roots_with_external_source(
                    ExternalRootSource::HardFork(hard_fork_restart_slot),
                    self.blockstore,
                    &mut self.original_blockstore_root,
                )
                .map_err(|err| format!("Failed to reconcile blockstore with hard fork: {err:?}"))?;
            }

            *self.start_progress.write().unwrap() = previous_start_process;
        }
        Ok(())
    }

    pub(crate) fn process_to_create_tower(mut self) -> Result<Tower, String> {
        self.process()?;
        Ok(self.tower.unwrap())
    }
}

fn maybe_warp_slot(
    config: &ValidatorConfig,
    process_blockstore: &mut ProcessBlockStore,
    ledger_path: &Path,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    snapshot_controller: &SnapshotController,
) -> Result<(), String> {
    if let Some(warp_slot) = config.warp_slot {
        let mut bank_forks = bank_forks.write().unwrap();

        let working_bank = bank_forks.working_bank();

        if warp_slot <= working_bank.slot() {
            return Err(format!(
                "warp slot ({}) cannot be less than the working bank slot ({})",
                warp_slot,
                working_bank.slot()
            ));
        }
        info!("warping to slot {warp_slot}");

        let root_bank = bank_forks.root_bank();

        // An accounts hash calculation from storages will occur in warp_from_parent() below.  This
        // requires that the accounts cache has been flushed, which requires the parent slot to be
        // rooted.
        root_bank.squash();
        root_bank.force_flush_accounts_cache();

        bank_forks.insert(Bank::warp_from_parent(
            root_bank,
            &Pubkey::default(),
            warp_slot,
        ));
        bank_forks.set_root(warp_slot, Some(snapshot_controller), Some(warp_slot));
        leader_schedule_cache.set_root(&bank_forks.root_bank());

        let full_snapshot_archive_info = match snapshot_bank_utils::bank_to_full_snapshot_archive(
            ledger_path,
            &bank_forks.root_bank(),
            None,
            &config.snapshot_config.full_snapshot_archives_dir,
            &config.snapshot_config.incremental_snapshot_archives_dir,
            config.snapshot_config.archive_format,
        ) {
            Ok(archive_info) => archive_info,
            Err(e) => return Err(format!("Unable to create snapshot: {e}")),
        };
        info!(
            "created snapshot: {}",
            full_snapshot_archive_info.path().display()
        );

        drop(bank_forks);
        // Process blockstore after warping bank forks to make sure tower and
        // bank forks are in sync.
        process_blockstore.process()?;
    }
    Ok(())
}

/// Returns the starting slot at which the blockstore should be scanned for
/// shreds with an incorrect shred version, or None if the check is unnecessary
fn should_cleanup_blockstore_incorrect_shred_versions(
    config: &ValidatorConfig,
    blockstore: &Blockstore,
    root_slot: Slot,
    hard_forks: &HardForks,
) -> Result<Option<Slot>, BlockstoreError> {
    // Perform the check if we are booting as part of a cluster restart at slot root_slot
    let maybe_cluster_restart_slot = maybe_cluster_restart_with_hard_fork(config, root_slot);
    if maybe_cluster_restart_slot.is_some() {
        return Ok(Some(root_slot + 1));
    }

    // If there are no hard forks, the shred version cannot have changed
    let Some(latest_hard_fork) = hard_forks.iter().last().map(|(slot, _)| *slot) else {
        return Ok(None);
    };

    // If the blockstore is empty, there are certainly no shreds with an incorrect version
    let Some(blockstore_max_slot) = blockstore.highest_slot()? else {
        return Ok(None);
    };
    let blockstore_min_slot = blockstore.lowest_slot();
    info!(
        "Blockstore contains data from slot {blockstore_min_slot} to {blockstore_max_slot}, the \
         latest hard fork is {latest_hard_fork}"
    );

    if latest_hard_fork < blockstore_min_slot {
        // latest_hard_fork < blockstore_min_slot <= blockstore_max_slot
        //
        // All slots in the blockstore are newer than the latest hard fork, and only shreds with
        // the correct shred version should have been inserted since the latest hard fork
        //
        // This is the normal case where the last cluster restart & hard fork was a while ago; we
        // can skip the check for this case
        Ok(None)
    } else if latest_hard_fork < blockstore_max_slot {
        // blockstore_min_slot < latest_hard_fork < blockstore_max_slot
        //
        // This could be a case where there was a cluster restart, but this node was not part of
        // the supermajority that actually restarted the cluster. Rather, this node likely
        // downloaded a new snapshot while retaining the blockstore, including slots beyond the
        // chosen restart slot. We need to perform the blockstore check for this case
        //
        // Note that the downloaded snapshot slot (root_slot) could be greater than the latest hard
        // fork slot. Even though this node will only replay slots after root_slot, start the check
        // at latest_hard_fork + 1 to check (and possibly purge) any invalid state.
        Ok(Some(latest_hard_fork + 1))
    } else {
        // blockstore_min_slot <= blockstore_max_slot <= latest_hard_fork
        //
        // All slots in the blockstore are older than the latest hard fork. The blockstore check
        // would start from latest_hard_fork + 1; skip the check as there are no slots to check
        //
        // This is kind of an unusual case to hit, maybe a node has been offline for a long time
        // and just restarted with a new downloaded snapshot but the old blockstore
        Ok(None)
    }
}

/// Searches the blockstore for data shreds with a shred version that differs
/// from the passed `expected_shred_version`
fn scan_blockstore_for_incorrect_shred_version(
    blockstore: &Blockstore,
    start_slot: Slot,
    expected_shred_version: u16,
) -> Result<Option<u16>, BlockstoreError> {
    const TIMEOUT: Duration = Duration::from_secs(60);
    let timer = Instant::now();
    // Search for shreds with incompatible version in blockstore
    let slot_meta_iterator = blockstore.slot_meta_iterator(start_slot)?;

    info!("Searching blockstore for shred with incorrect version from slot {start_slot}");
    for (slot, _meta) in slot_meta_iterator {
        let shreds = blockstore.get_data_shreds_for_slot(slot, 0)?;
        for shred in &shreds {
            if shred.version() != expected_shred_version {
                return Ok(Some(shred.version()));
            }
        }
        if timer.elapsed() > TIMEOUT {
            info!("Didn't find incorrect shreds after 60 seconds, aborting");
            break;
        }
    }
    Ok(None)
}

/// If the blockstore contains any shreds with the incorrect shred version,
/// copy them to a backup blockstore and purge them from the actual blockstore.
fn cleanup_blockstore_incorrect_shred_versions(
    blockstore: &Blockstore,
    config: &ValidatorConfig,
    start_slot: Slot,
    expected_shred_version: u16,
) -> Result<(), BlockstoreError> {
    let incorrect_shred_version = scan_blockstore_for_incorrect_shred_version(
        blockstore,
        start_slot,
        expected_shred_version,
    )?;
    let Some(incorrect_shred_version) = incorrect_shred_version else {
        info!("Only shreds with the correct version were found in the blockstore");
        return Ok(());
    };

    // .unwrap() safe because getting to this point implies blockstore has slots/shreds
    let end_slot = blockstore.highest_slot()?.unwrap();

    // Backing up the shreds that will be deleted from primary blockstore is
    // not critical, so swallow errors from backup blockstore operations.
    let backup_folder = format!(
        "{BLOCKSTORE_DIRECTORY_ROCKS_LEVEL}_backup_{incorrect_shred_version}_{start_slot}_{end_slot}"
    );
    match Blockstore::open_with_options(
        &blockstore.ledger_path().join(backup_folder),
        config.blockstore_options.clone(),
    ) {
        Ok(backup_blockstore) => {
            info!("Backing up slots from {start_slot} to {end_slot}");
            let mut timer = Measure::start("blockstore backup");

            const PRINT_INTERVAL: Duration = Duration::from_secs(5);
            let mut print_timer = Instant::now();
            let mut num_slots_copied = 0;
            let slot_meta_iterator = blockstore.slot_meta_iterator(start_slot)?;
            for (slot, _meta) in slot_meta_iterator {
                let shreds = blockstore.get_data_shreds_for_slot(slot, 0)?;
                let shreds = shreds.into_iter().map(Cow::Owned);
                let _ = backup_blockstore.insert_cow_shreds(shreds, None, true);
                num_slots_copied += 1;

                if print_timer.elapsed() > PRINT_INTERVAL {
                    info!("Backed up {num_slots_copied} slots thus far");
                    print_timer = Instant::now();
                }
            }

            timer.stop();
            info!("Backing up slots done. {timer}");
        }
        Err(err) => {
            warn!("Unable to backup shreds with incorrect shred version: {err}");
        }
    }

    info!("Purging slots {start_slot} to {end_slot} from blockstore");
    let mut timer = Measure::start("blockstore purge");
    blockstore.purge_from_next_slots(start_slot, end_slot);
    blockstore.purge_slots(start_slot, end_slot, PurgeType::Exact);
    timer.stop();
    info!("Purging slots done. {timer}");

    Ok(())
}

fn initialize_rpc_transaction_history_services(
    blockstore: Arc<Blockstore>,
    exit: Arc<AtomicBool>,
    enable_rpc_transaction_history: bool,
    enable_extended_tx_metadata_storage: bool,
    transaction_notifier: Option<TransactionNotifierArc>,
    dependency_tracker: Option<Arc<DependencyTracker>>,
) -> TransactionHistoryServices {
    let max_complete_transaction_status_slot = Arc::new(AtomicU64::new(blockstore.max_root()));
    let (transaction_status_sender, transaction_status_receiver) = unbounded();
    let transaction_status_sender = Some(TransactionStatusSender {
        sender: transaction_status_sender,
        dependency_tracker: dependency_tracker.clone(),
    });
    let transaction_status_service = Some(TransactionStatusService::new(
        transaction_status_receiver,
        max_complete_transaction_status_slot.clone(),
        enable_rpc_transaction_history,
        transaction_notifier,
        blockstore.clone(),
        enable_extended_tx_metadata_storage,
        dependency_tracker,
        exit.clone(),
    ));

    TransactionHistoryServices {
        transaction_status_sender,
        transaction_status_service,
        max_complete_transaction_status_slot,
    }
}

#[derive(Error, Debug)]
pub enum ValidatorError {
    #[error("bank hash mismatch: actual={0}, expected={1}")]
    BankHashMismatch(Hash, Hash),

    #[error("blockstore error: {0}")]
    Blockstore(#[source] BlockstoreError),

    #[error("genesis hash mismatch: actual={0}, expected={1}")]
    GenesisHashMismatch(Hash, Hash),

    #[error(
        "ledger does not have enough data to wait for supermajority: current slot={0}, needed \
         slot={1}"
    )]
    NotEnoughLedgerData(Slot, Slot),

    #[error("failed to open genesis: {0}")]
    OpenGenesisConfig(#[source] OpenGenesisConfigError),

    #[error("{0}")]
    Other(String),

    #[error(
        "PoH hashes/second rate is slower than the cluster target: mine {mine}, cluster {target}"
    )]
    PohTooSlow { mine: u64, target: u64 },

    #[error(transparent)]
    ResourceLimitError(#[from] ResourceLimitError),

    #[error("shred version mismatch: actual {actual}, expected {expected}")]
    ShredVersionMismatch { actual: u16, expected: u16 },

    #[error(transparent)]
    TraceError(#[from] TraceError),

    #[error("Wen Restart finished, please continue with --wait-for-supermajority")]
    WenRestartFinished,
}

// Return if the validator waited on other nodes to start. In this case
// it should not wait for one of it's votes to land to produce blocks
// because if the whole network is waiting, then it will stall.
//
// Error indicates that a bad hash was encountered or another condition
// that is unrecoverable and the validator should exit.
fn wait_for_supermajority(
    config: &ValidatorConfig,
    process_blockstore: Option<&mut ProcessBlockStore>,
    bank_forks: &RwLock<BankForks>,
    cluster_info: &ClusterInfo,
    rpc_override_health_check: Arc<AtomicBool>,
    start_progress: &Arc<RwLock<ValidatorStartProgress>>,
) -> Result<bool, ValidatorError> {
    match config.wait_for_supermajority {
        None => Ok(false),
        Some(wait_for_supermajority_slot) => {
            if let Some(process_blockstore) = process_blockstore {
                process_blockstore
                    .process()
                    .map_err(ValidatorError::Other)?;
            }

            let bank = bank_forks.read().unwrap().working_bank();
            match wait_for_supermajority_slot.cmp(&bank.slot()) {
                std::cmp::Ordering::Less => return Ok(false),
                std::cmp::Ordering::Greater => {
                    return Err(ValidatorError::NotEnoughLedgerData(
                        bank.slot(),
                        wait_for_supermajority_slot,
                    ));
                }
                _ => {}
            }

            if let Some(expected_bank_hash) = config.expected_bank_hash {
                if bank.hash() != expected_bank_hash {
                    return Err(ValidatorError::BankHashMismatch(
                        bank.hash(),
                        expected_bank_hash,
                    ));
                }
            }

            for i in 1.. {
                let logging = i % 10 == 1;
                if logging {
                    info!(
                        "Waiting for {}% of activated stake at slot {} to be in gossip...",
                        WAIT_FOR_SUPERMAJORITY_THRESHOLD_PERCENT,
                        bank.slot()
                    );
                }

                let gossip_stake_percent =
                    get_stake_percent_in_gossip(&bank, cluster_info, logging);

                *start_progress.write().unwrap() =
                    ValidatorStartProgress::WaitingForSupermajority {
                        slot: wait_for_supermajority_slot,
                        gossip_stake_percent,
                    };

                if gossip_stake_percent >= WAIT_FOR_SUPERMAJORITY_THRESHOLD_PERCENT {
                    info!(
                        "Supermajority reached, {gossip_stake_percent}% active stake detected, \
                         starting up now.",
                    );
                    break;
                }
                // The normal RPC health checks don't apply as the node is waiting, so feign health to
                // prevent load balancers from removing the node from their list of candidates during a
                // manual restart.
                rpc_override_health_check.store(true, Ordering::Relaxed);
                thread::sleep(Duration::new(1, 0));
            }
            rpc_override_health_check.store(false, Ordering::Relaxed);
            Ok(true)
        }
    }
}

// Get the activated stake percentage (based on the provided bank) that is visible in gossip
fn get_stake_percent_in_gossip(bank: &Bank, cluster_info: &ClusterInfo, log: bool) -> u64 {
    let mut online_stake = 0;
    let mut wrong_shred_stake = 0;
    let mut wrong_shred_nodes = vec![];
    let mut offline_stake = 0;
    let mut offline_nodes = vec![];

    let mut total_activated_stake = 0;
    let now = timestamp();
    // Nodes contact infos are saved to disk and restored on validator startup.
    // Staked nodes entries will not expire until an epoch after. So it
    // is necessary here to filter for recent entries to establish liveness.
    let peers: HashMap<_, _> = cluster_info
        .tvu_peers(ContactInfo::clone)
        .into_iter()
        .filter(|node| {
            let age = now.saturating_sub(node.wallclock());
            // Contact infos are refreshed twice during this period.
            age < CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS
        })
        .map(|node| (*node.pubkey(), node))
        .collect();
    let my_shred_version = cluster_info.my_shred_version();
    let my_id = cluster_info.id();

    for (activated_stake, vote_account) in bank.vote_accounts().values() {
        let activated_stake = *activated_stake;
        total_activated_stake += activated_stake;

        if activated_stake == 0 {
            continue;
        }
        let vote_state_node_pubkey = *vote_account.node_pubkey();

        if let Some(peer) = peers.get(&vote_state_node_pubkey) {
            if peer.shred_version() == my_shred_version {
                trace!(
                    "observed {vote_state_node_pubkey} in gossip, \
                     (activated_stake={activated_stake})"
                );
                online_stake += activated_stake;
            } else {
                wrong_shred_stake += activated_stake;
                wrong_shred_nodes.push((activated_stake, vote_state_node_pubkey));
            }
        } else if vote_state_node_pubkey == my_id {
            online_stake += activated_stake; // This node is online
        } else {
            offline_stake += activated_stake;
            offline_nodes.push((activated_stake, vote_state_node_pubkey));
        }
    }

    let online_stake_percentage = (online_stake as f64 / total_activated_stake as f64) * 100.;
    if log {
        info!("{online_stake_percentage:.3}% of active stake visible in gossip");

        if !wrong_shred_nodes.is_empty() {
            info!(
                "{:.3}% of active stake has the wrong shred version in gossip",
                (wrong_shred_stake as f64 / total_activated_stake as f64) * 100.,
            );
            wrong_shred_nodes.sort_by(|b, a| a.0.cmp(&b.0)); // sort by reverse stake weight
            for (stake, identity) in wrong_shred_nodes {
                info!(
                    "    {:.3}% - {}",
                    (stake as f64 / total_activated_stake as f64) * 100.,
                    identity
                );
            }
        }

        if !offline_nodes.is_empty() {
            info!(
                "{:.3}% of active stake is not visible in gossip",
                (offline_stake as f64 / total_activated_stake as f64) * 100.
            );
            offline_nodes.sort_by(|b, a| a.0.cmp(&b.0)); // sort by reverse stake weight
            for (stake, identity) in offline_nodes {
                info!(
                    "    {:.3}% - {}",
                    (stake as f64 / total_activated_stake as f64) * 100.,
                    identity
                );
            }
        }
    }

    online_stake_percentage as u64
}

fn cleanup_accounts_paths(config: &ValidatorConfig) {
    for account_path in &config.account_paths {
        move_and_async_delete_path_contents(account_path);
    }
    if let Some(shrink_paths) = &config.accounts_db_config.shrink_paths {
        for shrink_path in shrink_paths {
            move_and_async_delete_path_contents(shrink_path);
        }
    }
}

pub fn is_snapshot_config_valid(snapshot_config: &SnapshotConfig) -> bool {
    // if the snapshot config is configured to *not* take snapshots, then it is valid
    if !snapshot_config.should_generate_snapshots() {
        return true;
    }

    let SnapshotInterval::Slots(full_snapshot_interval_slots) =
        snapshot_config.full_snapshot_archive_interval
    else {
        // if we *are* generating snapshots, then the full snapshot interval cannot be disabled
        return false;
    };

    match snapshot_config.incremental_snapshot_archive_interval {
        SnapshotInterval::Disabled => true,
        SnapshotInterval::Slots(incremental_snapshot_interval_slots) => {
            full_snapshot_interval_slots > incremental_snapshot_interval_slots
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::{bounded, RecvTimeoutError},
        solana_entry::entry,
        solana_genesis_config::create_genesis_config,
        solana_gossip::contact_info::ContactInfo,
        solana_ledger::{
            blockstore, create_new_tmp_ledger, genesis_utils::create_genesis_config_with_leader,
            get_tmp_ledger_path_auto_delete,
        },
        solana_poh_config::PohConfig,
        solana_sha256_hasher::hash,
        solana_tpu_client::tpu_client::DEFAULT_TPU_ENABLE_UDP,
        std::{fs::remove_dir_all, num::NonZeroU64, thread, time::Duration},
    };

    #[test]
    fn validator_exit() {
        agave_logger::setup();
        let leader_keypair = Keypair::new();
        let leader_node = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());

        let validator_keypair = Keypair::new();
        let validator_node = Node::new_localhost_with_pubkey(&validator_keypair.pubkey());
        let genesis_config =
            create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1000)
                .genesis_config;
        let (validator_ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);

        let voting_keypair = Arc::new(Keypair::new());
        let config = ValidatorConfig {
            rpc_addrs: Some((
                validator_node.info.rpc().unwrap(),
                validator_node.info.rpc_pubsub().unwrap(),
            )),
            ..ValidatorConfig::default_for_test()
        };
        let start_progress = Arc::new(RwLock::new(ValidatorStartProgress::default()));
        let validator = Validator::new(
            validator_node,
            Arc::new(validator_keypair),
            &validator_ledger_path,
            &voting_keypair.pubkey(),
            Arc::new(RwLock::new(vec![voting_keypair])),
            vec![leader_node.info],
            &config,
            true, // should_check_duplicate_instance
            None, // rpc_to_plugin_manager_receiver
            start_progress.clone(),
            SocketAddrSpace::Unspecified,
            ValidatorTpuConfig::new_for_tests(DEFAULT_TPU_ENABLE_UDP),
            Arc::new(RwLock::new(None)),
        )
        .expect("assume successful validator start");
        assert_eq!(
            *start_progress.read().unwrap(),
            ValidatorStartProgress::Running
        );
        validator.close();
        remove_dir_all(validator_ledger_path).unwrap();
    }

    #[test]
    fn test_should_cleanup_blockstore_incorrect_shred_versions() {
        agave_logger::setup();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let mut validator_config = ValidatorConfig::default_for_test();
        let mut hard_forks = HardForks::default();
        let mut root_slot;

        // Do check from root_slot + 1 if wait_for_supermajority (10) == root_slot (10)
        root_slot = 10;
        validator_config.wait_for_supermajority = Some(root_slot);
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            Some(root_slot + 1)
        );

        // No check if wait_for_supermajority (10) < root_slot (15) (no hard forks)
        // Arguably operator error to pass a value for wait_for_supermajority in this case
        root_slot = 15;
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            None,
        );

        // Emulate cluster restart at slot 10
        // No check if wait_for_supermajority (10) < root_slot (15) (empty blockstore)
        hard_forks.register(10);
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            None,
        );

        // Insert some shreds at newer slots than hard fork
        let entries = entry::create_ticks(1, 0, Hash::default());
        for i in 20..35 {
            let shreds = blockstore::entries_to_test_shreds(
                &entries,
                i,     // slot
                i - 1, // parent_slot
                true,  // is_full_slot
                1,     // version
            );
            blockstore.insert_shreds(shreds, None, true).unwrap();
        }

        // No check as all blockstore data is newer than latest hard fork
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            None,
        );

        // Emulate cluster restart at slot 25
        // Do check from root_slot + 1 regardless of whether wait_for_supermajority set correctly
        root_slot = 25;
        hard_forks.register(root_slot);
        validator_config.wait_for_supermajority = Some(root_slot);
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            Some(root_slot + 1),
        );
        validator_config.wait_for_supermajority = None;
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            Some(root_slot + 1),
        );

        // Do check with advanced root slot, even without wait_for_supermajority set correctly
        // Check starts from latest hard fork + 1
        root_slot = 30;
        let latest_hard_fork = hard_forks.iter().last().unwrap().0;
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            Some(latest_hard_fork + 1),
        );

        // Purge blockstore up to latest hard fork
        // No check since all blockstore data newer than latest hard fork
        blockstore.purge_slots(0, latest_hard_fork, PurgeType::Exact);
        assert_eq!(
            should_cleanup_blockstore_incorrect_shred_versions(
                &validator_config,
                &blockstore,
                root_slot,
                &hard_forks
            )
            .unwrap(),
            None,
        );
    }

    #[test]
    fn test_cleanup_blockstore_incorrect_shred_versions() {
        agave_logger::setup();

        let validator_config = ValidatorConfig::default_for_test();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let entries = entry::create_ticks(1, 0, Hash::default());
        for i in 1..10 {
            let shreds = blockstore::entries_to_test_shreds(
                &entries,
                i,     // slot
                i - 1, // parent_slot
                true,  // is_full_slot
                1,     // version
            );
            blockstore.insert_shreds(shreds, None, true).unwrap();
        }

        // this purges and compacts all slots greater than or equal to 5
        cleanup_blockstore_incorrect_shred_versions(&blockstore, &validator_config, 5, 2).unwrap();
        // assert that slots less than 5 aren't affected
        assert!(blockstore.meta(4).unwrap().unwrap().next_slots.is_empty());
        for i in 5..10 {
            assert!(blockstore
                .get_data_shreds_for_slot(i, 0)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn validator_parallel_exit() {
        let leader_keypair = Keypair::new();
        let leader_node = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());
        let genesis_config =
            create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1000)
                .genesis_config;

        let mut ledger_paths = vec![];
        let mut validators: Vec<Validator> = (0..2)
            .map(|_| {
                let validator_keypair = Keypair::new();
                let validator_node = Node::new_localhost_with_pubkey(&validator_keypair.pubkey());
                let (validator_ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);
                ledger_paths.push(validator_ledger_path.clone());
                let vote_account_keypair = Keypair::new();
                let config = ValidatorConfig {
                    rpc_addrs: Some((
                        validator_node.info.rpc().unwrap(),
                        validator_node.info.rpc_pubsub().unwrap(),
                    )),
                    ..ValidatorConfig::default_for_test()
                };
                Validator::new(
                    validator_node,
                    Arc::new(validator_keypair),
                    &validator_ledger_path,
                    &vote_account_keypair.pubkey(),
                    Arc::new(RwLock::new(vec![Arc::new(vote_account_keypair)])),
                    vec![leader_node.info.clone()],
                    &config,
                    true, // should_check_duplicate_instance.
                    None, // rpc_to_plugin_manager_receiver
                    Arc::new(RwLock::new(ValidatorStartProgress::default())),
                    SocketAddrSpace::Unspecified,
                    ValidatorTpuConfig::new_for_tests(DEFAULT_TPU_ENABLE_UDP),
                    Arc::new(RwLock::new(None)),
                )
                .expect("assume successful validator start")
            })
            .collect();

        // Each validator can exit in parallel to speed many sequential calls to join`
        validators.iter_mut().for_each(|v| v.exit());

        // spawn a new thread to wait for the join of the validator
        let (sender, receiver) = bounded(0);
        let _ = thread::spawn(move || {
            validators.into_iter().for_each(|validator| {
                validator.join();
            });
            sender.send(()).unwrap();
        });

        let timeout = Duration::from_secs(60);
        if let Err(RecvTimeoutError::Timeout) = receiver.recv_timeout(timeout) {
            panic!("timeout for shutting down validators",);
        }

        for path in ledger_paths {
            remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn test_wait_for_supermajority() {
        agave_logger::setup();
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
            SocketAddrSpace::Unspecified,
        );

        let (genesis_config, _mint_keypair) = create_genesis_config(1);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let mut config = ValidatorConfig::default_for_test();
        let rpc_override_health_check = Arc::new(AtomicBool::new(false));
        let start_progress = Arc::new(RwLock::new(ValidatorStartProgress::default()));

        assert!(!wait_for_supermajority(
            &config,
            None,
            &bank_forks,
            &cluster_info,
            rpc_override_health_check.clone(),
            &start_progress,
        )
        .unwrap());

        // bank=0, wait=1, should fail
        config.wait_for_supermajority = Some(1);
        assert!(matches!(
            wait_for_supermajority(
                &config,
                None,
                &bank_forks,
                &cluster_info,
                rpc_override_health_check.clone(),
                &start_progress,
            ),
            Err(ValidatorError::NotEnoughLedgerData(_, _)),
        ));

        // bank=1, wait=0, should pass, bank is past the wait slot
        let bank_forks = BankForks::new_rw_arc(Bank::new_from_parent(
            bank_forks.read().unwrap().root_bank(),
            &Pubkey::default(),
            1,
        ));
        config.wait_for_supermajority = Some(0);
        assert!(!wait_for_supermajority(
            &config,
            None,
            &bank_forks,
            &cluster_info,
            rpc_override_health_check.clone(),
            &start_progress,
        )
        .unwrap());

        // bank=1, wait=1, equal, but bad hash provided
        config.wait_for_supermajority = Some(1);
        config.expected_bank_hash = Some(hash(&[1]));
        assert!(matches!(
            wait_for_supermajority(
                &config,
                None,
                &bank_forks,
                &cluster_info,
                rpc_override_health_check,
                &start_progress,
            ),
            Err(ValidatorError::BankHashMismatch(_, _)),
        ));
    }

    #[test]
    fn test_is_snapshot_config_valid() {
        fn new_snapshot_config(
            full_snapshot_archive_interval_slots: Slot,
            incremental_snapshot_archive_interval_slots: Slot,
        ) -> SnapshotConfig {
            SnapshotConfig {
                full_snapshot_archive_interval: SnapshotInterval::Slots(
                    NonZeroU64::new(full_snapshot_archive_interval_slots).unwrap(),
                ),
                incremental_snapshot_archive_interval: SnapshotInterval::Slots(
                    NonZeroU64::new(incremental_snapshot_archive_interval_slots).unwrap(),
                ),
                ..SnapshotConfig::default()
            }
        }

        // default config must be valid
        assert!(is_snapshot_config_valid(&SnapshotConfig::default()));

        // disabled incremental snapshot must be valid
        assert!(is_snapshot_config_valid(&SnapshotConfig {
            incremental_snapshot_archive_interval: SnapshotInterval::Disabled,
            ..SnapshotConfig::default()
        }));

        // disabled full snapshot must be invalid though (if generating snapshots)
        assert!(!is_snapshot_config_valid(&SnapshotConfig {
            full_snapshot_archive_interval: SnapshotInterval::Disabled,
            ..SnapshotConfig::default()
        }));

        // simple config must be valid
        assert!(is_snapshot_config_valid(&new_snapshot_config(400, 200)));
        assert!(is_snapshot_config_valid(&new_snapshot_config(100, 42)));
        assert!(is_snapshot_config_valid(&new_snapshot_config(444, 200)));
        assert!(is_snapshot_config_valid(&new_snapshot_config(400, 222)));

        // config where full interval is not larger than incremental interval must be invalid
        assert!(!is_snapshot_config_valid(&new_snapshot_config(42, 100)));
        assert!(!is_snapshot_config_valid(&new_snapshot_config(100, 100)));
        assert!(!is_snapshot_config_valid(&new_snapshot_config(100, 200)));

        // config with snapshots disabled (or load-only) must be valid
        assert!(is_snapshot_config_valid(&SnapshotConfig::new_disabled()));
        assert!(is_snapshot_config_valid(&SnapshotConfig::new_load_only()));
        assert!(is_snapshot_config_valid(&SnapshotConfig {
            full_snapshot_archive_interval: SnapshotInterval::Slots(NonZeroU64::new(37).unwrap()),
            incremental_snapshot_archive_interval: SnapshotInterval::Slots(
                NonZeroU64::new(41).unwrap()
            ),
            ..SnapshotConfig::new_load_only()
        }));
        assert!(is_snapshot_config_valid(&SnapshotConfig {
            full_snapshot_archive_interval: SnapshotInterval::Disabled,
            incremental_snapshot_archive_interval: SnapshotInterval::Disabled,
            ..SnapshotConfig::new_load_only()
        }));
    }

    fn target_tick_duration() -> Duration {
        // DEFAULT_MS_PER_SLOT = 400
        // DEFAULT_TICKS_PER_SLOT = 64
        // MS_PER_TICK = 6
        //
        // But, DEFAULT_MS_PER_SLOT / DEFAULT_TICKS_PER_SLOT = 6.25
        //
        // So, convert to microseconds first to avoid the integer rounding error
        let target_tick_duration_us =
            solana_clock::DEFAULT_MS_PER_SLOT * 1000 / solana_clock::DEFAULT_TICKS_PER_SLOT;
        assert_eq!(target_tick_duration_us, 6250);
        Duration::from_micros(target_tick_duration_us)
    }

    #[test]
    fn test_poh_speed() {
        agave_logger::setup();
        let poh_config = PohConfig {
            target_tick_duration: target_tick_duration(),
            // make PoH rate really fast to cause the panic condition
            hashes_per_tick: Some(100 * solana_clock::DEFAULT_HASHES_PER_TICK),
            ..PohConfig::default()
        };
        let genesis_config = GenesisConfig {
            poh_config,
            ..GenesisConfig::default()
        };
        let bank = Bank::new_for_tests(&genesis_config);
        assert!(check_poh_speed(&bank, Some(10_000)).is_err());
    }

    #[test]
    fn test_poh_speed_no_hashes_per_tick() {
        agave_logger::setup();
        let poh_config = PohConfig {
            target_tick_duration: target_tick_duration(),
            hashes_per_tick: None,
            ..PohConfig::default()
        };
        let genesis_config = GenesisConfig {
            poh_config,
            ..GenesisConfig::default()
        };
        let bank = Bank::new_for_tests(&genesis_config);
        check_poh_speed(&bank, Some(10_000)).unwrap();
    }
}
