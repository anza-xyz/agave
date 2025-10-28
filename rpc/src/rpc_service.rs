//! The `rpc_service` module implements the Solana JSON RPC service.

use {
    crate::{
        cluster_tpu_info::ClusterTpuInfo,
        max_slots::MaxSlots,
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        rpc::{rpc_accounts::*, rpc_accounts_scan::*, rpc_bank::*, rpc_full::*, rpc_minimal::*, *},
        rpc_cache::LargestAccountsCache,
        rpc_health::*,
    },
    agave_snapshots::{
        paths as snapshot_paths, snapshot_archive_info::SnapshotArchiveInfoGetter,
        snapshot_config::SnapshotConfig, SnapshotInterval,
    },
    crossbeam_channel::unbounded,
    jsonrpc_core::{futures::prelude::*, MetaIoHandler},
    jsonrpc_http_server::{
        hyper, AccessControlAllowOrigin, CloseHandle, DomainsValidation, RequestMiddleware,
        RequestMiddlewareAction, ServerBuilder,
    },
    regex::Regex,
    solana_cli_output::display::build_balance_message,
    solana_client::{
        client_option::ClientOption,
        connection_cache::{ConnectionCache, Protocol},
    },
    solana_genesis_config::DEFAULT_GENESIS_DOWNLOAD_PATH,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_ledger::{
        bigtable_upload::ConfirmedBlockUploadConfig,
        bigtable_upload_service::BigTableUploadService, blockstore::Blockstore,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_metrics::inc_new_counter_info,
    solana_perf::thread::renice_this_thread,
    solana_poh::poh_recorder::PohRecorder,
    solana_quic_definitions::NotifyKeyUpdate,
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, commitment::BlockCommitmentCache,
        non_circulating_supply::calculate_non_circulating_supply,
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_send_transaction_service::{
        send_transaction_service::{self, SendTransactionService},
        transaction_client::{ConnectionCacheClient, TpuClientNextClient, TransactionClient},
    },
    solana_storage_bigtable::CredentialType,
    solana_validator_exit::Exit,
    std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        task::{Context, Poll},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::runtime::{Builder as TokioBuilder, Runtime as TokioRuntime},
    tokio_util::{
        bytes::Bytes,
        codec::{BytesCodec, FramedRead},
    },
};

const FULL_SNAPSHOT_REQUEST_PATH: &str = "/snapshot.tar.bz2";
const INCREMENTAL_SNAPSHOT_REQUEST_PATH: &str = "/incremental-snapshot.tar.bz2";
const LARGEST_ACCOUNTS_CACHE_DURATION: u64 = 60 * 60 * 2;
/// Default minimum snapshot download speed is 10 MB/s
/// Full snapshots are ~90 GB, incremental are ~1 GB today but both will increase over time
/// Full: 120 GB / 10 MB/s = 12,000 seconds -> ~30k slots
const FALLBACK_FULL_SNAPSHOT_TIMEOUT_SECS: Duration = Duration::from_secs(12_000);
/// Incremental: 2.5 GB / 10 MB/s = 250 seconds -> ~625 slots
const FALLBACK_INCREMENTAL_SNAPSHOT_TIMEOUT_SECS: Duration = Duration::from_secs(250);

enum SnapshotKind {
    Full,
    Incremental,
}

struct TimeoutStream<S> {
    inner: S,
    deadline: Instant,
}

impl<S> TimeoutStream<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: Instant::now() + timeout,
        }
    }
}

impl<S> Stream for TimeoutStream<S>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if Instant::now() >= self.deadline {
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "snapshot transfer deadline exceeded",
            ))));
        }
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

pub struct JsonRpcService {
    thread_hdl: JoinHandle<()>,

    #[cfg(test)]
    pub request_processor: JsonRpcRequestProcessor, // Used only by test_rpc_new()...

    close_handle: Option<CloseHandle>,

    client_updater: Arc<dyn NotifyKeyUpdate + Send + Sync>,
}

struct RpcRequestMiddleware {
    ledger_path: PathBuf,
    full_snapshot_archive_path_regex: Regex,
    incremental_snapshot_archive_path_regex: Regex,
    snapshot_config: Option<SnapshotConfig>,
    bank_forks: Arc<RwLock<BankForks>>,
    health: Arc<RpcHealth>,
}

impl RpcRequestMiddleware {
    pub fn new(
        ledger_path: PathBuf,
        snapshot_config: Option<SnapshotConfig>,
        bank_forks: Arc<RwLock<BankForks>>,
        health: Arc<RpcHealth>,
    ) -> Self {
        Self {
            ledger_path,
            full_snapshot_archive_path_regex: Regex::new(
                snapshot_paths::FULL_SNAPSHOT_ARCHIVE_FILENAME_REGEX,
            )
            .unwrap(),
            incremental_snapshot_archive_path_regex: Regex::new(
                snapshot_paths::INCREMENTAL_SNAPSHOT_ARCHIVE_FILENAME_REGEX,
            )
            .unwrap(),
            snapshot_config,
            bank_forks,
            health,
        }
    }

    fn redirect(location: &str) -> hyper::Response<hyper::Body> {
        hyper::Response::builder()
            .status(hyper::StatusCode::SEE_OTHER)
            .header(hyper::header::LOCATION, location)
            .body(hyper::Body::from(String::from(location)))
            .unwrap()
    }

    fn not_found() -> hyper::Response<hyper::Body> {
        hyper::Response::builder()
            .status(hyper::StatusCode::NOT_FOUND)
            .body(hyper::Body::empty())
            .unwrap()
    }

    fn internal_server_error() -> hyper::Response<hyper::Body> {
        hyper::Response::builder()
            .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
            .body(hyper::Body::empty())
            .unwrap()
    }

    fn strip_leading_slash(path: &str) -> Option<&str> {
        path.strip_prefix('/')
    }

    fn is_file_get_path(&self, path: &str) -> bool {
        if path == DEFAULT_GENESIS_DOWNLOAD_PATH {
            return true;
        }

        if self.snapshot_config.is_none() {
            return false;
        }

        let Some(path) = Self::strip_leading_slash(path) else {
            return false;
        };

        self.full_snapshot_archive_path_regex.is_match(path)
            || self.incremental_snapshot_archive_path_regex.is_match(path)
    }

    #[cfg(unix)]
    async fn open_no_follow(path: impl AsRef<Path>) -> std::io::Result<tokio::fs::File> {
        tokio::fs::OpenOptions::new()
            .read(true)
            .write(false)
            .create(false)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await
    }

    #[cfg(not(unix))]
    async fn open_no_follow(path: impl AsRef<Path>) -> std::io::Result<tokio::fs::File> {
        // TODO: Is there any way to achieve the same on Windows?
        tokio::fs::File::open(path).await
    }

    fn find_snapshot_file<P>(&self, stem: P) -> (PathBuf, SnapshotKind)
    where
        P: AsRef<Path>,
    {
        let is_full = self
            .full_snapshot_archive_path_regex
            .is_match(Path::new("").join(&stem).to_str().unwrap());
        let root = if is_full {
            &self
                .snapshot_config
                .as_ref()
                .unwrap()
                .full_snapshot_archives_dir
        } else {
            &self
                .snapshot_config
                .as_ref()
                .unwrap()
                .incremental_snapshot_archives_dir
        };
        let local_path = root.join(&stem);
        let path = if local_path.exists() {
            local_path
        } else {
            // remote snapshot archive path
            snapshot_paths::build_snapshot_archives_remote_dir(root).join(stem)
        };
        (
            path,
            if is_full {
                SnapshotKind::Full
            } else {
                SnapshotKind::Incremental
            },
        )
    }

    fn process_file_get(&self, path: &str) -> RequestMiddlewareAction {
        let (filename, snapshot_type) = {
            let stem = Self::strip_leading_slash(path).expect("path already verified");
            match path {
                DEFAULT_GENESIS_DOWNLOAD_PATH => {
                    inc_new_counter_info!("rpc-get_genesis", 1);
                    (self.ledger_path.join(stem), None)
                }
                _ => {
                    inc_new_counter_info!("rpc-get_snapshot", 1);
                    let (path, snapshot_type) = self.find_snapshot_file(stem);
                    (path, Some(snapshot_type))
                }
            }
        };
        let file_length = std::fs::metadata(&filename)
            .map(|m| m.len())
            .unwrap_or(0)
            .to_string();
        info!("get {path} -> {filename:?} ({file_length} bytes)");

        if cfg!(not(test)) {
            assert!(
                self.snapshot_config.is_some(),
                "snapshot_config should never be None outside of tests"
            );
        }
        let snapshot_timeout = self.snapshot_config.as_ref().and_then(|config| {
            snapshot_type.map(|st| {
                let interval = match st {
                    SnapshotKind::Full => config.full_snapshot_archive_interval,
                    SnapshotKind::Incremental => config.incremental_snapshot_archive_interval,
                };
                let computed = match interval {
                    SnapshotInterval::Disabled => Duration::ZERO,
                    SnapshotInterval::Slots(slots) => Duration::from_millis(
                        slots
                            .get()
                            .saturating_mul(solana_clock::DEFAULT_MS_PER_SLOT),
                    ),
                };
                let fallback = match st {
                    SnapshotKind::Full => FALLBACK_FULL_SNAPSHOT_TIMEOUT_SECS,
                    SnapshotKind::Incremental => FALLBACK_INCREMENTAL_SNAPSHOT_TIMEOUT_SECS,
                };
                std::cmp::max(computed, fallback)
            })
        });

        RequestMiddlewareAction::Respond {
            should_validate_hosts: true,
            response: Box::pin(async move {
                match Self::open_no_follow(filename).await {
                    Err(err) => Ok(if err.kind() == std::io::ErrorKind::NotFound {
                        Self::not_found()
                    } else {
                        Self::internal_server_error()
                    }),
                    Ok(file) => {
                        let stream =
                            FramedRead::new(file, BytesCodec::new()).map_ok(|b| b.freeze());
                        let body = if let Some(timeout) = snapshot_timeout {
                            hyper::Body::wrap_stream(TimeoutStream::new(stream, timeout))
                        } else {
                            hyper::Body::wrap_stream(stream)
                        };
                        Ok(hyper::Response::builder()
                            .header(hyper::header::CONTENT_LENGTH, file_length)
                            .body(body)
                            .unwrap())
                    }
                }
            }),
        }
    }

    fn health_check(&self) -> &'static str {
        let response = match self.health.check() {
            RpcHealthStatus::Ok => "ok",
            RpcHealthStatus::Behind { .. } => "behind",
            RpcHealthStatus::Unknown => "unknown",
        };
        info!("health check: {response}");
        response
    }
}

impl RequestMiddleware for RpcRequestMiddleware {
    fn on_request(&self, request: hyper::Request<hyper::Body>) -> RequestMiddlewareAction {
        trace!("request uri: {}", request.uri());

        if let Some(ref snapshot_config) = self.snapshot_config {
            if request.uri().path() == FULL_SNAPSHOT_REQUEST_PATH
                || request.uri().path() == INCREMENTAL_SNAPSHOT_REQUEST_PATH
            {
                // Convenience redirect to the latest snapshot
                let full_snapshot_archive_info =
                    snapshot_paths::get_highest_full_snapshot_archive_info(
                        &snapshot_config.full_snapshot_archives_dir,
                    );
                let snapshot_archive_info =
                    if let Some(full_snapshot_archive_info) = full_snapshot_archive_info {
                        if request.uri().path() == FULL_SNAPSHOT_REQUEST_PATH {
                            Some(full_snapshot_archive_info.snapshot_archive_info().clone())
                        } else {
                            snapshot_paths::get_highest_incremental_snapshot_archive_info(
                                &snapshot_config.incremental_snapshot_archives_dir,
                                full_snapshot_archive_info.slot(),
                            )
                            .map(|incremental_snapshot_archive_info| {
                                incremental_snapshot_archive_info
                                    .snapshot_archive_info()
                                    .clone()
                            })
                        }
                    } else {
                        None
                    };
                return if let Some(snapshot_archive_info) = snapshot_archive_info {
                    RpcRequestMiddleware::redirect(&format!(
                        "/{}",
                        snapshot_archive_info
                            .path
                            .file_name()
                            .unwrap_or_else(|| std::ffi::OsStr::new(""))
                            .to_str()
                            .unwrap_or("")
                    ))
                } else {
                    RpcRequestMiddleware::not_found()
                }
                .into();
            }
        }

        if let Some(path) = match_supply_path(request.uri().path()) {
            process_rest(&self.bank_forks, path)
        } else if self.is_file_get_path(request.uri().path()) {
            self.process_file_get(request.uri().path())
        } else if request.uri().path() == "/health" {
            hyper::Response::builder()
                .status(hyper::StatusCode::OK)
                .body(hyper::Body::from(self.health_check()))
                .unwrap()
                .into()
        } else {
            request.into()
        }
    }
}

fn match_supply_path(path: &str) -> Option<&str> {
    match path {
        "/v0/circulating-supply" | "/v0/total-supply" => Some(path),
        _ => None,
    }
}

#[derive(Debug)]
pub enum SupplyCalcError {
    Scan(String),
}

async fn calculate_circulating_supply_async(bank: &Arc<Bank>) -> Result<u64, SupplyCalcError> {
    let total_supply = bank.capitalization();
    let bank = Arc::clone(bank);
    let non_circulating_supply =
        tokio::task::spawn_blocking(move || calculate_non_circulating_supply(&bank))
            .await
            .expect("Failed to spawn blocking task")
            .map_err(|e| SupplyCalcError::Scan(e.to_string()))?;

    Ok(total_supply.saturating_sub(non_circulating_supply.lamports))
}

async fn handle_rest(bank_forks: &Arc<RwLock<BankForks>>, path: &str) -> Option<String> {
    match path {
        "/v0/circulating-supply" => {
            let bank = bank_forks.read().unwrap().root_bank();
            let supply_result = calculate_circulating_supply_async(&bank).await;
            match supply_result {
                Ok(supply) => Some(build_balance_message(supply, false, false)),
                Err(_) => None,
            }
        }
        "/v0/total-supply" => {
            let bank = bank_forks.read().unwrap().root_bank();
            let total_supply = bank.capitalization();
            Some(build_balance_message(total_supply, false, false))
        }
        _ => None,
    }
}

fn process_rest(bank_forks: &Arc<RwLock<BankForks>>, path: &str) -> RequestMiddlewareAction {
    let bank_forks = bank_forks.clone();
    let path = path.to_string();

    RequestMiddlewareAction::Respond {
        should_validate_hosts: true,
        response: Box::pin(async move {
            let result = handle_rest(&bank_forks, path.as_str()).await;
            match result {
                Some(s) => Ok(hyper::Response::builder()
                    .status(hyper::StatusCode::OK)
                    .body(hyper::Body::from(s))
                    .unwrap()),
                None => Ok(RpcRequestMiddleware::not_found()),
            }
        }),
    }
}

/// [`JsonRpcServiceConfig`] is a helper structure that simplifies the creation
/// of a [`JsonRpcService`] with a target TPU client specified by
/// `client_option`.
pub struct JsonRpcServiceConfig<'a> {
    pub rpc_addr: SocketAddr,
    pub rpc_config: JsonRpcConfig,
    pub snapshot_config: Option<SnapshotConfig>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    pub blockstore: Arc<Blockstore>,
    pub cluster_info: Arc<ClusterInfo>,
    pub poh_recorder: Option<Arc<RwLock<PohRecorder>>>,
    pub genesis_hash: Hash,
    pub ledger_path: PathBuf,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub exit: Arc<AtomicBool>,
    pub override_health_check: Arc<AtomicBool>,
    pub optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    pub send_transaction_service_config: send_transaction_service::Config,
    pub max_slots: Arc<MaxSlots>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub max_complete_transaction_status_slot: Arc<AtomicU64>,
    pub prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    pub client_option: ClientOption<'a>,
}

impl JsonRpcService {
    pub fn new_with_config(config: JsonRpcServiceConfig) -> Result<Self, String> {
        let runtime = service_runtime(
            config.rpc_config.rpc_threads,
            config.rpc_config.rpc_blocking_threads,
            config.rpc_config.rpc_niceness_adj,
        );
        let leader_info = config
            .poh_recorder
            .map(|recorder| ClusterTpuInfo::new(config.cluster_info.clone(), recorder));

        match config.client_option {
            ClientOption::ConnectionCache(connection_cache) => {
                let my_tpu_address = config
                    .cluster_info
                    .my_contact_info()
                    .tpu(connection_cache.protocol())
                    .ok_or(format!(
                        "Invalid {:?} socket address for TPU",
                        connection_cache.protocol()
                    ))?;
                let client = ConnectionCacheClient::new(
                    connection_cache,
                    my_tpu_address,
                    config.send_transaction_service_config.tpu_peers.clone(),
                    leader_info,
                    config.send_transaction_service_config.leader_forward_count,
                );
                let json_rpc_service = Self::new_with_client(
                    config.rpc_addr,
                    config.rpc_config,
                    config.snapshot_config,
                    config.bank_forks,
                    config.block_commitment_cache,
                    config.blockstore,
                    config.cluster_info,
                    config.genesis_hash,
                    config.ledger_path.as_path(),
                    config.validator_exit,
                    config.exit,
                    config.override_health_check,
                    config.optimistically_confirmed_bank,
                    config.send_transaction_service_config,
                    config.max_slots,
                    config.leader_schedule_cache,
                    client.clone(),
                    config.max_complete_transaction_status_slot,
                    config.prioritization_fee_cache,
                    runtime,
                )?;
                Ok(json_rpc_service)
            }
            ClientOption::TpuClientNext(
                identity_keypair,
                tpu_client_socket,
                client_runtime,
                cancel,
            ) => {
                let my_tpu_address = config
                    .cluster_info
                    .my_contact_info()
                    .tpu(Protocol::QUIC)
                    .ok_or(format!(
                        "Invalid {:?} socket address for TPU",
                        Protocol::QUIC
                    ))?;
                let client = TpuClientNextClient::new(
                    client_runtime,
                    my_tpu_address,
                    config.send_transaction_service_config.tpu_peers.clone(),
                    leader_info,
                    config.send_transaction_service_config.leader_forward_count,
                    Some(identity_keypair),
                    tpu_client_socket,
                    cancel,
                );

                let json_rpc_service = Self::new_with_client(
                    config.rpc_addr,
                    config.rpc_config.clone(),
                    config.snapshot_config,
                    config.bank_forks.clone(),
                    config.block_commitment_cache.clone(),
                    config.blockstore.clone(),
                    config.cluster_info.clone(),
                    config.genesis_hash,
                    config.ledger_path.as_path(),
                    config.validator_exit,
                    config.exit,
                    config.override_health_check,
                    config.optimistically_confirmed_bank,
                    config.send_transaction_service_config,
                    config.max_slots,
                    config.leader_schedule_cache,
                    client,
                    config.max_complete_transaction_status_slot,
                    config.prioritization_fee_cache,
                    runtime,
                )?;
                Ok(json_rpc_service)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc_addr: SocketAddr,
        config: JsonRpcConfig,
        snapshot_config: Option<SnapshotConfig>,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Option<Arc<RwLock<PohRecorder>>>,
        genesis_hash: Hash,
        ledger_path: &Path,
        validator_exit: Arc<RwLock<Exit>>,
        exit: Arc<AtomicBool>,
        override_health_check: Arc<AtomicBool>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
        send_transaction_service_config: send_transaction_service::Config,
        max_slots: Arc<MaxSlots>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        connection_cache: Arc<ConnectionCache>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> Result<Self, String> {
        let runtime = service_runtime(
            config.rpc_threads,
            config.rpc_blocking_threads,
            config.rpc_niceness_adj,
        );

        let tpu_address = cluster_info
            .my_contact_info()
            .tpu(connection_cache.protocol())
            .ok_or_else(|| {
                format!(
                    "Invalid {:?} socket address for TPU",
                    connection_cache.protocol()
                )
            })?;

        let leader_info =
            poh_recorder.map(|recorder| ClusterTpuInfo::new(cluster_info.clone(), recorder));
        let client = ConnectionCacheClient::new(
            connection_cache,
            tpu_address,
            send_transaction_service_config.tpu_peers.clone(),
            leader_info,
            send_transaction_service_config.leader_forward_count,
        );
        let json_rpc_service = Self::new_with_client(
            rpc_addr,
            config,
            snapshot_config,
            bank_forks,
            block_commitment_cache,
            blockstore,
            cluster_info,
            genesis_hash,
            ledger_path,
            validator_exit,
            exit,
            override_health_check,
            optimistically_confirmed_bank,
            send_transaction_service_config,
            max_slots,
            leader_schedule_cache,
            client.clone(),
            max_complete_transaction_status_slot,
            prioritization_fee_cache,
            runtime,
        )?;
        Ok(json_rpc_service)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_client<
        Client: TransactionClient
            + NotifyKeyUpdate
            + Clone
            + std::marker::Send
            + std::marker::Sync
            + 'static,
    >(
        rpc_addr: SocketAddr,
        config: JsonRpcConfig,
        snapshot_config: Option<SnapshotConfig>,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        genesis_hash: Hash,
        ledger_path: &Path,
        validator_exit: Arc<RwLock<Exit>>,
        exit: Arc<AtomicBool>,
        override_health_check: Arc<AtomicBool>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
        send_transaction_service_config: send_transaction_service::Config,
        max_slots: Arc<MaxSlots>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        client: Client,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
        runtime: Arc<TokioRuntime>,
    ) -> Result<Self, String> {
        info!("rpc bound to {rpc_addr:?}");
        info!("rpc configuration: {config:?}");
        let rpc_niceness_adj = config.rpc_niceness_adj;

        let health = Arc::new(RpcHealth::new(
            Arc::clone(&optimistically_confirmed_bank),
            Arc::clone(&blockstore),
            config.health_check_slot_distance,
            override_health_check,
        ));

        let largest_accounts_cache = Arc::new(RwLock::new(LargestAccountsCache::new(
            LARGEST_ACCOUNTS_CACHE_DURATION,
        )));

        let exit_bigtable_ledger_upload_service = Arc::new(AtomicBool::new(false));

        let (bigtable_ledger_storage, _bigtable_ledger_upload_service) =
            if let Some(RpcBigtableConfig {
                enable_bigtable_ledger_upload,
                ref bigtable_instance_name,
                ref bigtable_app_profile_id,
                timeout,
                max_message_size,
            }) = config.rpc_bigtable_config
            {
                let bigtable_config = solana_storage_bigtable::LedgerStorageConfig {
                    read_only: !enable_bigtable_ledger_upload,
                    timeout,
                    credential_type: CredentialType::Filepath(None),
                    instance_name: bigtable_instance_name.clone(),
                    app_profile_id: bigtable_app_profile_id.clone(),
                    max_message_size,
                };
                runtime
                    .block_on(solana_storage_bigtable::LedgerStorage::new_with_config(
                        bigtable_config,
                    ))
                    .map(|bigtable_ledger_storage| {
                        info!("BigTable ledger storage initialized");

                        let bigtable_ledger_upload_service = if enable_bigtable_ledger_upload {
                            Some(Arc::new(BigTableUploadService::new_with_config(
                                runtime.clone(),
                                bigtable_ledger_storage.clone(),
                                blockstore.clone(),
                                block_commitment_cache.clone(),
                                max_complete_transaction_status_slot.clone(),
                                ConfirmedBlockUploadConfig::default(),
                                exit_bigtable_ledger_upload_service.clone(),
                            )))
                        } else {
                            None
                        };

                        (
                            Some(bigtable_ledger_storage),
                            bigtable_ledger_upload_service,
                        )
                    })
                    .unwrap_or_else(|err| {
                        error!("Failed to initialize BigTable ledger storage: {err:?}");
                        (None, None)
                    })
            } else {
                (None, None)
            };

        let full_api = config.full_api;
        let max_request_body_size = config
            .max_request_body_size
            .unwrap_or(MAX_REQUEST_BODY_SIZE);
        let (request_processor, receiver) = JsonRpcRequestProcessor::new(
            config,
            snapshot_config.clone(),
            bank_forks.clone(),
            block_commitment_cache,
            blockstore,
            validator_exit.clone(),
            health.clone(),
            cluster_info.clone(),
            genesis_hash,
            bigtable_ledger_storage,
            optimistically_confirmed_bank,
            largest_accounts_cache,
            max_slots,
            leader_schedule_cache,
            max_complete_transaction_status_slot,
            prioritization_fee_cache,
            Arc::clone(&runtime),
        );

        let _send_transaction_service = Arc::new(SendTransactionService::new_with_client(
            &bank_forks,
            receiver,
            client.clone(),
            send_transaction_service_config,
            exit,
        ));

        #[cfg(test)]
        let test_request_processor = request_processor.clone();

        let ledger_path = ledger_path.to_path_buf();

        let (close_handle_sender, close_handle_receiver) = unbounded();
        let thread_hdl = Builder::new()
            .name("solJsonRpcSvc".to_string())
            .spawn(move || {
                renice_this_thread(rpc_niceness_adj).unwrap();

                let mut io = MetaIoHandler::default();

                io.extend_with(rpc_minimal::MinimalImpl.to_delegate());
                if full_api {
                    io.extend_with(rpc_bank::BankDataImpl.to_delegate());
                    io.extend_with(rpc_accounts::AccountsDataImpl.to_delegate());
                    io.extend_with(rpc_accounts_scan::AccountsScanImpl.to_delegate());
                    io.extend_with(rpc_full::FullImpl.to_delegate());
                }

                let request_middleware = RpcRequestMiddleware::new(
                    ledger_path,
                    snapshot_config,
                    bank_forks.clone(),
                    health.clone(),
                );
                let server = ServerBuilder::with_meta_extractor(
                    io,
                    move |req: &hyper::Request<hyper::Body>| {
                        let xbigtable = req.headers().get("x-bigtable");
                        if xbigtable.is_some_and(|v| v == "disabled") {
                            request_processor.clone_without_bigtable()
                        } else {
                            request_processor.clone()
                        }
                    },
                )
                .event_loop_executor(runtime.handle().clone())
                .threads(1)
                .cors(DomainsValidation::AllowOnly(vec![
                    AccessControlAllowOrigin::Any,
                ]))
                .cors_max_age(86400)
                .request_middleware(request_middleware)
                .max_request_body_size(max_request_body_size)
                .start_http(&rpc_addr);

                if let Err(e) = server {
                    warn!(
                        "JSON RPC service unavailable error: {e:?}. Also, check that port {} is \
                         not already in use by another application",
                        rpc_addr.port()
                    );
                    close_handle_sender.send(Err(e.to_string())).unwrap();
                    return;
                }

                let server = server.unwrap();
                close_handle_sender.send(Ok(server.close_handle())).unwrap();
                server.wait();
                exit_bigtable_ledger_upload_service.store(true, Ordering::Relaxed);
            })
            .unwrap();

        let close_handle = close_handle_receiver.recv().unwrap()?;
        let close_handle_ = close_handle.clone();
        validator_exit
            .write()
            .unwrap()
            .register_exit(Box::new(move || {
                close_handle_.close();
            }));
        Ok(Self {
            thread_hdl,
            #[cfg(test)]
            request_processor: test_request_processor,
            close_handle: Some(close_handle),
            client_updater: Arc::new(client) as Arc<dyn NotifyKeyUpdate + Send + Sync>,
        })
    }

    pub fn exit(&mut self) {
        if let Some(c) = self.close_handle.take() {
            c.close()
        }
    }

    pub fn join(mut self) -> thread::Result<()> {
        self.exit();
        self.thread_hdl.join()
    }

    pub fn get_client_key_updater(&self) -> Arc<dyn NotifyKeyUpdate + Send + Sync> {
        self.client_updater.clone()
    }
}

pub fn service_runtime(
    rpc_threads: usize,
    rpc_blocking_threads: usize,
    rpc_niceness_adj: i8,
) -> Arc<TokioRuntime> {
    // The jsonrpc_http_server crate supports two execution models:
    //
    // - By default, it spawns a number of threads - configured with .threads(N) - and runs a
    //   single-threaded futures executor in each thread.
    // - Alternatively when configured with .event_loop_executor(executor) and .threads(1),
    //   it executes all the tasks on the given executor, not spawning any extra internal threads.
    //
    // We use the latter configuration, using a multi threaded tokio runtime as the executor. We
    // do this so we can configure the number of worker threads, the number of blocking threads
    // and then use tokio::task::spawn_blocking() to avoid blocking the worker threads on CPU
    // bound operations like getMultipleAccounts. This results in reduced latency, since fast
    // rpc calls (the majority) are not blocked by slow CPU bound ones.
    //
    // NB: `rpc_blocking_threads` shouldn't be set too high (defaults to num_cpus / 2). Too many
    // (busy) blocking threads could compete with CPU time with other validator threads and
    // negatively impact performance.
    let rpc_threads = 1.max(rpc_threads);
    let rpc_blocking_threads = 1.max(rpc_blocking_threads);
    let runtime = Arc::new(
        TokioBuilder::new_multi_thread()
            .worker_threads(rpc_threads)
            .max_blocking_threads(rpc_blocking_threads)
            .on_thread_start(move || renice_this_thread(rpc_niceness_adj).unwrap())
            .thread_name("solRpcEl")
            .enable_all()
            .build()
            .expect("Runtime"),
    );
    runtime
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::rpc::{create_validator_exit, tests::new_test_cluster_info},
        solana_cluster_type::ClusterType,
        solana_genesis_config::DEFAULT_GENESIS_ARCHIVE,
        solana_ledger::{
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            get_tmp_ledger_path_auto_delete,
        },
        solana_rpc_client_api::config::RpcContextConfig,
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        std::{
            io::Write,
            net::{IpAddr, Ipv4Addr},
        },
        tokio::runtime::Runtime,
    };

    #[test]
    fn test_rpc_new() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let exit = Arc::new(AtomicBool::new(false));
        let validator_exit = create_validator_exit(exit.clone());
        let bank = Bank::new_for_tests(&genesis_config);
        let cluster_info = Arc::new(new_test_cluster_info());
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let port_range = solana_net_utils::sockets::localhost_port_range_for_tests();
        let rpc_addr = SocketAddr::new(
            ip_addr,
            solana_net_utils::find_available_port_in_range(ip_addr, port_range).unwrap(),
        );
        let bank_forks = BankForks::new_rw_arc(bank);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let connection_cache = Arc::new(ConnectionCache::new("connection_cache_test"));
        let mut rpc_service = JsonRpcService::new(
            rpc_addr,
            JsonRpcConfig::default(),
            None,
            bank_forks,
            block_commitment_cache,
            blockstore,
            cluster_info,
            None,
            Hash::default(),
            &PathBuf::from("farf"),
            validator_exit,
            exit,
            Arc::new(AtomicBool::new(false)),
            optimistically_confirmed_bank,
            send_transaction_service::Config {
                retry_rate_ms: 1000,
                leader_forward_count: 1,
                ..send_transaction_service::Config::default()
            },
            Arc::new(MaxSlots::default()),
            Arc::new(LeaderScheduleCache::default()),
            connection_cache,
            Arc::new(AtomicU64::default()),
            Arc::new(PrioritizationFeeCache::default()),
        )
        .expect("assume successful JsonRpcService start");
        let thread = rpc_service.thread_hdl.thread();
        assert_eq!(thread.name().unwrap(), "solJsonRpcSvc");

        assert_eq!(
            10_000,
            rpc_service
                .request_processor
                .get_balance(&mint_keypair.pubkey(), RpcContextConfig::default())
                .unwrap()
                .value
        );
        rpc_service.exit();
        rpc_service.join().unwrap();
    }

    fn create_bank_forks() -> Arc<RwLock<BankForks>> {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(10_000);
        genesis_config.cluster_type = ClusterType::MainnetBeta;
        let bank = Bank::new_for_tests(&genesis_config);
        BankForks::new_rw_arc(bank)
    }

    #[test]
    fn test_process_rest_api() {
        let bank_forks = create_bank_forks();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            assert_eq!(
                None,
                handle_rest(&bank_forks, "not-a-supported-rest-api").await
            );

            let circulating_supply = handle_rest(&bank_forks, "/v0/circulating-supply").await;
            assert!(circulating_supply.is_some());

            let total_supply = handle_rest(&bank_forks, "/v0/total-supply").await;
            assert!(total_supply.is_some());

            assert_eq!(
                handle_rest(&bank_forks, "/v0/circulating-supply").await,
                handle_rest(&bank_forks, "/v0/total-supply").await
            );
        });
    }

    #[test]
    fn test_strip_prefix() {
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("/"), Some(""));
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("//"), Some("/"));
        assert_eq!(
            RpcRequestMiddleware::strip_leading_slash("/abc"),
            Some("abc")
        );
        assert_eq!(
            RpcRequestMiddleware::strip_leading_slash("//abc"),
            Some("/abc")
        );
        assert_eq!(
            RpcRequestMiddleware::strip_leading_slash("/./abc"),
            Some("./abc")
        );
        assert_eq!(
            RpcRequestMiddleware::strip_leading_slash("/../abc"),
            Some("../abc")
        );

        assert_eq!(RpcRequestMiddleware::strip_leading_slash(""), None);
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("./"), None);
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("../"), None);
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("."), None);
        assert_eq!(RpcRequestMiddleware::strip_leading_slash(".."), None);
        assert_eq!(RpcRequestMiddleware::strip_leading_slash("abc"), None);
    }

    #[test]
    fn test_is_file_get_path() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let bank_forks = create_bank_forks();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let health = RpcHealth::stub(optimistically_confirmed_bank, blockstore);

        let bank_forks = create_bank_forks();
        let rrm = RpcRequestMiddleware::new(
            ledger_path.path().to_path_buf(),
            None,
            bank_forks.clone(),
            health.clone(),
        );
        let rrm_with_snapshot_config = RpcRequestMiddleware::new(
            ledger_path.path().to_path_buf(),
            Some(SnapshotConfig::default()),
            bank_forks,
            health,
        );

        assert!(rrm.is_file_get_path(DEFAULT_GENESIS_DOWNLOAD_PATH));
        assert!(!rrm.is_file_get_path(DEFAULT_GENESIS_ARCHIVE));
        assert!(!rrm.is_file_get_path("//genesis.tar.bz2"));
        assert!(!rrm.is_file_get_path("/../genesis.tar.bz2"));

        // These two are redirects
        assert!(!rrm.is_file_get_path("/snapshot.tar.bz2"));
        assert!(!rrm.is_file_get_path("/incremental-snapshot.tar.bz2"));

        assert!(!rrm.is_file_get_path(
            "/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));

        assert!(rrm_with_snapshot_config.is_file_get_path(
            "/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(rrm_with_snapshot_config.is_file_get_path(
            "/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.lz4"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.bz2"
        ));
        assert!(!rrm_with_snapshot_config
            .is_file_get_path("/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.gz"));
        assert!(!rrm_with_snapshot_config
            .is_file_get_path("/snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar"));

        assert!(rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.lz4"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.bz2"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.gz"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar"
        ));

        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/snapshot-notaslotnumber-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-notaslotnumber-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/incremental-snapshot-100-notaslotnumber-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));

        assert!(
            !rrm_with_snapshot_config.is_file_get_path("../../../test/snapshot-123-xxx.tar.zst")
        );
        assert!(!rrm_with_snapshot_config
            .is_file_get_path("../../../test/incremental-snapshot-123-456-xxx.tar.zst"));

        assert!(!rrm.is_file_get_path("/"));
        assert!(!rrm.is_file_get_path("//"));
        assert!(!rrm.is_file_get_path("/."));
        assert!(!rrm.is_file_get_path("/./"));
        assert!(!rrm.is_file_get_path("/.."));
        assert!(!rrm.is_file_get_path("/../"));
        assert!(!rrm.is_file_get_path("."));
        assert!(!rrm.is_file_get_path("./"));
        assert!(!rrm.is_file_get_path(".//"));
        assert!(!rrm.is_file_get_path(".."));
        assert!(!rrm.is_file_get_path("../"));
        assert!(!rrm.is_file_get_path("..//"));
        assert!(!rrm.is_file_get_path("🎣"));

        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "//snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/./snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/../snapshot-100-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "//incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/./incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
        assert!(!rrm_with_snapshot_config.is_file_get_path(
            "/../incremental-snapshot-100-200-AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr.tar.zst"
        ));
    }

    #[test]
    fn test_process_file_get() {
        let runtime = Runtime::new().unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let genesis_path = ledger_path.path().join(DEFAULT_GENESIS_ARCHIVE);
        let bank_forks = create_bank_forks();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let rrm = RpcRequestMiddleware::new(
            ledger_path.path().to_path_buf(),
            None,
            bank_forks,
            RpcHealth::stub(optimistically_confirmed_bank, blockstore),
        );

        // File does not exist => request should fail.
        let action = rrm.process_file_get(DEFAULT_GENESIS_DOWNLOAD_PATH);
        if let RequestMiddlewareAction::Respond { response, .. } = action {
            let response = runtime.block_on(response);
            let response = response.unwrap();
            assert_ne!(response.status(), 200);
        } else {
            panic!("Unexpected RequestMiddlewareAction variant");
        }

        {
            let mut file = std::fs::File::create(&genesis_path).unwrap();
            file.write_all(b"should be ok").unwrap();
        }

        // Normal file exist => request should succeed.
        let action = rrm.process_file_get(DEFAULT_GENESIS_DOWNLOAD_PATH);
        if let RequestMiddlewareAction::Respond { response, .. } = action {
            let response = runtime.block_on(response);
            let response = response.unwrap();
            assert_eq!(response.status(), 200);
        } else {
            panic!("Unexpected RequestMiddlewareAction variant");
        }

        std::fs::remove_file(&genesis_path).unwrap();
        {
            let mut file = std::fs::File::create(ledger_path.path().join("wrong")).unwrap();
            file.write_all(b"wrong file").unwrap();
        }
        symlink::symlink_file("wrong", &genesis_path).unwrap();

        // File is a symbolic link => request should fail.
        let action = rrm.process_file_get(DEFAULT_GENESIS_DOWNLOAD_PATH);
        if let RequestMiddlewareAction::Respond { response, .. } = action {
            let response = runtime.block_on(response);
            let response = response.unwrap();
            assert_ne!(response.status(), 200);
        } else {
            panic!("Unexpected RequestMiddlewareAction variant");
        }
    }
}
