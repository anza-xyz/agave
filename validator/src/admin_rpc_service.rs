use {
    crossbeam_channel::Sender,
    jsonrpc_core::{BoxFuture, ErrorCode, MetaIoHandler, Metadata, Result},
    jsonrpc_core_client::{transports::ipc, RpcError},
    jsonrpc_derive::rpc,
    jsonrpc_ipc_server::{
        tokio::sync::oneshot::channel as oneshot_channel, RequestContext, ServerBuilder,
    },
    log::*,
    serde::{de::Deserializer, Deserialize, Serialize},
    solana_accounts_db::accounts_index::AccountIndex,
    solana_core::{
        admin_rpc_post_init::AdminRpcRequestMetadataPostInit,
        banking_stage::{
            transaction_scheduler::scheduler_controller::SchedulerConfig, BankingControlMsg,
            BankingStage,
        },
        consensus::{tower_storage::TowerStorage, Tower},
        repair::repair_service,
        validator::{
            BlockProductionMethod, SchedulerPacing, TransactionStructure, ValidatorStartProgress,
        },
    },
    solana_geyser_plugin_manager::GeyserPluginManagerRequest,
    solana_gossip::contact_info::{ContactInfo, Protocol, SOCKET_ADDR_UNSPECIFIED},
    solana_keypair::{read_keypair_file, Keypair},
    solana_pubkey::Pubkey,
    solana_rpc::rpc::verify_pubkey,
    solana_rpc_client_api::{config::RpcAccountIndex, custom_error::RpcCustomError},
    solana_signer::Signer,
    solana_validator_exit::Exit,
    std::{
        collections::{HashMap, HashSet},
        env, error,
        fmt::{self, Display},
        net::{IpAddr, SocketAddr},
        num::NonZeroUsize,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder},
        time::{Duration, SystemTime},
    },
    tokio::runtime::Runtime,
};

#[derive(Clone)]
pub struct AdminRpcRequestMetadata {
    pub rpc_addr: Option<SocketAddr>,
    pub start_time: SystemTime,
    pub start_progress: Arc<RwLock<ValidatorStartProgress>>,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub validator_exit_backpressure: HashMap<String, Arc<AtomicBool>>,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub tower_storage: Arc<dyn TowerStorage>,
    pub staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
    pub post_init: Arc<RwLock<Option<AdminRpcRequestMetadataPostInit>>>,
    pub rpc_to_plugin_manager_sender: Option<Sender<GeyserPluginManagerRequest>>,
}

impl Metadata for AdminRpcRequestMetadata {}

impl AdminRpcRequestMetadata {
    fn with_post_init<F, R>(&self, func: F) -> Result<R>
    where
        F: FnOnce(&AdminRpcRequestMetadataPostInit) -> Result<R>,
    {
        if let Some(post_init) = self.post_init.read().unwrap().as_ref() {
            func(post_init)
        } else {
            Err(jsonrpc_core::error::Error::invalid_params(
                "Retry once validator start up is complete",
            ))
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AdminRpcContactInfo {
    pub id: String,
    pub gossip: SocketAddr,
    pub tvu: SocketAddr,
    pub tvu_quic: SocketAddr,
    pub serve_repair_quic: SocketAddr,
    pub tpu: SocketAddr,
    pub tpu_forwards: SocketAddr,
    pub tpu_vote: SocketAddr,
    pub rpc: SocketAddr,
    pub rpc_pubsub: SocketAddr,
    pub serve_repair: SocketAddr,
    pub last_updated_timestamp: u64,
    pub shred_version: u16,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AdminRpcRepairWhitelist {
    pub whitelist: Vec<Pubkey>,
}

impl From<ContactInfo> for AdminRpcContactInfo {
    fn from(node: ContactInfo) -> Self {
        macro_rules! unwrap_socket {
            ($name:ident) => {
                node.$name().unwrap_or(SOCKET_ADDR_UNSPECIFIED)
            };
            ($name:ident, $protocol:expr) => {
                node.$name($protocol).unwrap_or(SOCKET_ADDR_UNSPECIFIED)
            };
        }
        Self {
            id: node.pubkey().to_string(),
            last_updated_timestamp: node.wallclock(),
            gossip: unwrap_socket!(gossip),
            tvu: unwrap_socket!(tvu, Protocol::UDP),
            tvu_quic: unwrap_socket!(tvu, Protocol::QUIC),
            serve_repair_quic: unwrap_socket!(serve_repair, Protocol::QUIC),
            tpu: unwrap_socket!(tpu, Protocol::UDP),
            tpu_forwards: unwrap_socket!(tpu_forwards, Protocol::UDP),
            tpu_vote: unwrap_socket!(tpu_vote, Protocol::UDP),
            rpc: unwrap_socket!(rpc),
            rpc_pubsub: unwrap_socket!(rpc_pubsub),
            serve_repair: unwrap_socket!(serve_repair, Protocol::UDP),
            shred_version: node.shred_version(),
        }
    }
}

impl Display for AdminRpcContactInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Identity: {}", self.id)?;
        writeln!(f, "Gossip: {}", self.gossip)?;
        writeln!(f, "TVU: {}", self.tvu)?;
        writeln!(f, "TVU QUIC: {}", self.tvu_quic)?;
        writeln!(f, "TPU: {}", self.tpu)?;
        writeln!(f, "TPU Forwards: {}", self.tpu_forwards)?;
        writeln!(f, "TPU Votes: {}", self.tpu_vote)?;
        writeln!(f, "RPC: {}", self.rpc)?;
        writeln!(f, "RPC Pubsub: {}", self.rpc_pubsub)?;
        writeln!(f, "Serve Repair: {}", self.serve_repair)?;
        writeln!(f, "Last Updated Timestamp: {}", self.last_updated_timestamp)?;
        writeln!(f, "Shred Version: {}", self.shred_version)
    }
}
impl solana_cli_output::VerboseDisplay for AdminRpcContactInfo {}
impl solana_cli_output::QuietDisplay for AdminRpcContactInfo {}

impl Display for AdminRpcRepairWhitelist {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Repair whitelist: {:?}", &self.whitelist)
    }
}
impl solana_cli_output::VerboseDisplay for AdminRpcRepairWhitelist {}
impl solana_cli_output::QuietDisplay for AdminRpcRepairWhitelist {}

#[rpc]
pub trait AdminRpc {
    type Metadata;

    /// Initiates validator exit; exit is asynchronous so the validator
    /// will almost certainly still be running when this method returns
    #[rpc(meta, name = "exit")]
    fn exit(&self, meta: Self::Metadata) -> Result<()>;

    /// Return the process id (pid)
    #[rpc(meta, name = "pid")]
    fn pid(&self, meta: Self::Metadata) -> Result<u32>;

    #[rpc(meta, name = "reloadPlugin")]
    fn reload_plugin(
        &self,
        meta: Self::Metadata,
        name: String,
        config_file: String,
    ) -> BoxFuture<Result<()>>;

    #[rpc(meta, name = "unloadPlugin")]
    fn unload_plugin(&self, meta: Self::Metadata, name: String) -> BoxFuture<Result<()>>;

    #[rpc(meta, name = "loadPlugin")]
    fn load_plugin(&self, meta: Self::Metadata, config_file: String) -> BoxFuture<Result<String>>;

    #[rpc(meta, name = "listPlugins")]
    fn list_plugins(&self, meta: Self::Metadata) -> BoxFuture<Result<Vec<String>>>;

    #[rpc(meta, name = "rpcAddress")]
    fn rpc_addr(&self, meta: Self::Metadata) -> Result<Option<SocketAddr>>;

    #[rpc(name = "setLogFilter")]
    fn set_log_filter(&self, filter: String) -> Result<()>;

    #[rpc(meta, name = "startTime")]
    fn start_time(&self, meta: Self::Metadata) -> Result<SystemTime>;

    #[rpc(meta, name = "startProgress")]
    fn start_progress(&self, meta: Self::Metadata) -> Result<ValidatorStartProgress>;

    #[rpc(meta, name = "addAuthorizedVoter")]
    fn add_authorized_voter(&self, meta: Self::Metadata, keypair_file: String) -> Result<()>;

    #[rpc(meta, name = "addAuthorizedVoterFromBytes")]
    fn add_authorized_voter_from_bytes(&self, meta: Self::Metadata, keypair: Vec<u8>)
        -> Result<()>;

    #[rpc(meta, name = "removeAllAuthorizedVoters")]
    fn remove_all_authorized_voters(&self, meta: Self::Metadata) -> Result<()>;

    #[rpc(meta, name = "setIdentity")]
    fn set_identity(
        &self,
        meta: Self::Metadata,
        keypair_file: String,
        require_tower: bool,
    ) -> Result<()>;

    #[rpc(meta, name = "setIdentityFromBytes")]
    fn set_identity_from_bytes(
        &self,
        meta: Self::Metadata,
        identity_keypair: Vec<u8>,
        require_tower: bool,
    ) -> Result<()>;

    #[rpc(meta, name = "setStakedNodesOverrides")]
    fn set_staked_nodes_overrides(&self, meta: Self::Metadata, path: String) -> Result<()>;

    #[rpc(meta, name = "contactInfo")]
    fn contact_info(&self, meta: Self::Metadata) -> Result<AdminRpcContactInfo>;

    #[rpc(meta, name = "selectActiveInterface")]
    fn select_active_interface(&self, meta: Self::Metadata, interface: IpAddr) -> Result<()>;

    #[rpc(meta, name = "repairShredFromPeer")]
    fn repair_shred_from_peer(
        &self,
        meta: Self::Metadata,
        pubkey: Option<Pubkey>,
        slot: u64,
        shred_index: u64,
    ) -> Result<()>;

    #[rpc(meta, name = "repairWhitelist")]
    fn repair_whitelist(&self, meta: Self::Metadata) -> Result<AdminRpcRepairWhitelist>;

    #[rpc(meta, name = "setRepairWhitelist")]
    fn set_repair_whitelist(&self, meta: Self::Metadata, whitelist: Vec<Pubkey>) -> Result<()>;

    #[rpc(meta, name = "getSecondaryIndexKeySize")]
    fn get_secondary_index_key_size(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
    ) -> Result<HashMap<RpcAccountIndex, usize>>;

    #[rpc(meta, name = "setPublicTpuAddress")]
    fn set_public_tpu_address(
        &self,
        meta: Self::Metadata,
        public_tpu_addr: SocketAddr,
    ) -> Result<()>;

    #[rpc(meta, name = "setPublicTpuForwardsAddress")]
    fn set_public_tpu_forwards_address(
        &self,
        meta: Self::Metadata,
        public_tpu_forwards_addr: SocketAddr,
    ) -> Result<()>;

    #[rpc(meta, name = "setPublicTvuAddress")]
    fn set_public_tvu_address(
        &self,
        meta: Self::Metadata,
        public_tvu_addr: SocketAddr,
    ) -> Result<()>;

    #[rpc(meta, name = "manageBlockProduction")]
    fn manage_block_production(
        &self,
        meta: Self::Metadata,
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
        num_workers: NonZeroUsize,
        scheduler_pacing: SchedulerPacing,
    ) -> Result<()>;
}

pub struct AdminRpcImpl;
impl AdminRpc for AdminRpcImpl {
    type Metadata = AdminRpcRequestMetadata;

    fn exit(&self, meta: Self::Metadata) -> Result<()> {
        debug!("exit admin rpc request received");

        thread::Builder::new()
            .name("solProcessExit".into())
            .spawn(move || {
                // Delay exit signal until this RPC request completes, otherwise the caller of `exit` might
                // receive a confusing error as the validator shuts down before a response is sent back.
                thread::sleep(Duration::from_millis(100));

                info!("validator exit requested");
                meta.validator_exit.write().unwrap().exit();

                if !meta.validator_exit_backpressure.is_empty() {
                    let service_names = meta.validator_exit_backpressure.keys();
                    info!("Wait for these services to complete: {service_names:?}");
                    loop {
                        // The initial sleep is a grace period to allow the services to raise their
                        // backpressure flags.
                        // Subsequent sleeps are to throttle how often we check and log.
                        thread::sleep(Duration::from_secs(1));

                        let mut any_flags_raised = false;
                        for (name, flag) in meta.validator_exit_backpressure.iter() {
                            let is_flag_raised = flag.load(Ordering::Relaxed);
                            if is_flag_raised {
                                info!("{name}'s exit backpressure flag is raised");
                                any_flags_raised = true;
                            }
                        }
                        if !any_flags_raised {
                            break;
                        }
                    }
                    info!("All services have completed");
                }

                // TODO: Debug why Exit doesn't always cause the validator to fully exit
                // (rocksdb background processing or some other stuck thread perhaps?).
                //
                // If the process is still alive after five seconds, exit harder
                thread::sleep(Duration::from_secs(
                    env::var("SOLANA_VALIDATOR_EXIT_TIMEOUT")
                        .ok()
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(5),
                ));
                warn!("validator exit timeout");
                std::process::exit(0);
            })
            .unwrap();

        Ok(())
    }

    fn pid(&self, _meta: Self::Metadata) -> Result<u32> {
        Ok(std::process::id())
    }

    fn reload_plugin(
        &self,
        meta: Self::Metadata,
        name: String,
        config_file: String,
    ) -> BoxFuture<Result<()>> {
        Box::pin(async move {
            // Construct channel for plugin to respond to this particular rpc request instance
            let (response_sender, response_receiver) = oneshot_channel();

            // Send request to plugin manager if there is a geyser service
            if let Some(ref rpc_to_manager_sender) = meta.rpc_to_plugin_manager_sender {
                rpc_to_manager_sender
                    .send(GeyserPluginManagerRequest::ReloadPlugin {
                        name,
                        config_file,
                        response_sender,
                    })
                    .expect("GeyerPluginService should never drop request receiver");
            } else {
                return Err(jsonrpc_core::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "No geyser plugin service".to_string(),
                    data: None,
                });
            }

            // Await response from plugin manager
            response_receiver
                .await
                .expect("GeyerPluginService's oneshot sender shouldn't drop early")
        })
    }

    fn load_plugin(&self, meta: Self::Metadata, config_file: String) -> BoxFuture<Result<String>> {
        Box::pin(async move {
            // Construct channel for plugin to respond to this particular rpc request instance
            let (response_sender, response_receiver) = oneshot_channel();

            // Send request to plugin manager if there is a geyser service
            if let Some(ref rpc_to_manager_sender) = meta.rpc_to_plugin_manager_sender {
                rpc_to_manager_sender
                    .send(GeyserPluginManagerRequest::LoadPlugin {
                        config_file,
                        response_sender,
                    })
                    .expect("GeyerPluginService should never drop request receiver");
            } else {
                return Err(jsonrpc_core::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "No geyser plugin service".to_string(),
                    data: None,
                });
            }

            // Await response from plugin manager
            response_receiver
                .await
                .expect("GeyerPluginService's oneshot sender shouldn't drop early")
        })
    }

    fn unload_plugin(&self, meta: Self::Metadata, name: String) -> BoxFuture<Result<()>> {
        Box::pin(async move {
            // Construct channel for plugin to respond to this particular rpc request instance
            let (response_sender, response_receiver) = oneshot_channel();

            // Send request to plugin manager if there is a geyser service
            if let Some(ref rpc_to_manager_sender) = meta.rpc_to_plugin_manager_sender {
                rpc_to_manager_sender
                    .send(GeyserPluginManagerRequest::UnloadPlugin {
                        name,
                        response_sender,
                    })
                    .expect("GeyerPluginService should never drop request receiver");
            } else {
                return Err(jsonrpc_core::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "No geyser plugin service".to_string(),
                    data: None,
                });
            }

            // Await response from plugin manager
            response_receiver
                .await
                .expect("GeyerPluginService's oneshot sender shouldn't drop early")
        })
    }

    fn list_plugins(&self, meta: Self::Metadata) -> BoxFuture<Result<Vec<String>>> {
        Box::pin(async move {
            // Construct channel for plugin to respond to this particular rpc request instance
            let (response_sender, response_receiver) = oneshot_channel();

            // Send request to plugin manager
            if let Some(ref rpc_to_manager_sender) = meta.rpc_to_plugin_manager_sender {
                rpc_to_manager_sender
                    .send(GeyserPluginManagerRequest::ListPlugins { response_sender })
                    .expect("GeyerPluginService should never drop request receiver");
            } else {
                return Err(jsonrpc_core::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "No geyser plugin service".to_string(),
                    data: None,
                });
            }

            // Await response from plugin manager
            response_receiver
                .await
                .expect("GeyerPluginService's oneshot sender shouldn't drop early")
        })
    }

    fn rpc_addr(&self, meta: Self::Metadata) -> Result<Option<SocketAddr>> {
        debug!("rpc_addr admin rpc request received");
        Ok(meta.rpc_addr)
    }

    fn set_log_filter(&self, filter: String) -> Result<()> {
        debug!("set_log_filter admin rpc request received");
        agave_logger::setup_with(&filter);
        Ok(())
    }

    fn start_time(&self, meta: Self::Metadata) -> Result<SystemTime> {
        debug!("start_time admin rpc request received");
        Ok(meta.start_time)
    }

    fn start_progress(&self, meta: Self::Metadata) -> Result<ValidatorStartProgress> {
        debug!("start_progress admin rpc request received");
        Ok(*meta.start_progress.read().unwrap())
    }

    fn add_authorized_voter(&self, meta: Self::Metadata, keypair_file: String) -> Result<()> {
        debug!("add_authorized_voter request received");

        let authorized_voter = read_keypair_file(keypair_file)
            .map_err(|err| jsonrpc_core::error::Error::invalid_params(format!("{err}")))?;

        AdminRpcImpl::add_authorized_voter_keypair(meta, authorized_voter)
    }

    fn add_authorized_voter_from_bytes(
        &self,
        meta: Self::Metadata,
        keypair: Vec<u8>,
    ) -> Result<()> {
        debug!("add_authorized_voter_from_bytes request received");

        let authorized_voter = Keypair::try_from(keypair.as_ref()).map_err(|err| {
            jsonrpc_core::error::Error::invalid_params(format!(
                "Failed to read authorized voter keypair from provided byte array: {err}"
            ))
        })?;

        AdminRpcImpl::add_authorized_voter_keypair(meta, authorized_voter)
    }

    fn remove_all_authorized_voters(&self, meta: Self::Metadata) -> Result<()> {
        debug!("remove_all_authorized_voters received");
        meta.authorized_voter_keypairs.write().unwrap().clear();
        Ok(())
    }

    fn set_identity(
        &self,
        meta: Self::Metadata,
        keypair_file: String,
        require_tower: bool,
    ) -> Result<()> {
        debug!("set_identity request received");

        let identity_keypair = read_keypair_file(&keypair_file).map_err(|err| {
            jsonrpc_core::error::Error::invalid_params(format!(
                "Failed to read identity keypair from {keypair_file}: {err}"
            ))
        })?;

        AdminRpcImpl::set_identity_keypair(meta, identity_keypair, require_tower)
    }

    fn set_identity_from_bytes(
        &self,
        meta: Self::Metadata,
        identity_keypair: Vec<u8>,
        require_tower: bool,
    ) -> Result<()> {
        debug!("set_identity_from_bytes request received");

        let identity_keypair = Keypair::try_from(identity_keypair.as_ref()).map_err(|err| {
            jsonrpc_core::error::Error::invalid_params(format!(
                "Failed to read identity keypair from provided byte array: {err}"
            ))
        })?;

        AdminRpcImpl::set_identity_keypair(meta, identity_keypair, require_tower)
    }

    fn set_staked_nodes_overrides(&self, meta: Self::Metadata, path: String) -> Result<()> {
        let loaded_config = load_staked_nodes_overrides(&path)
            .map_err(|err| {
                error!(
                    "Failed to load staked nodes overrides from {}: {}",
                    &path, err
                );
                jsonrpc_core::error::Error::internal_error()
            })?
            .staked_map_id;
        let mut write_staked_nodes = meta.staked_nodes_overrides.write().unwrap();
        write_staked_nodes.clear();
        write_staked_nodes.extend(loaded_config);
        info!("Staked nodes overrides loaded from {path}");
        debug!("overrides map: {write_staked_nodes:?}");
        Ok(())
    }

    fn contact_info(&self, meta: Self::Metadata) -> Result<AdminRpcContactInfo> {
        meta.with_post_init(|post_init| Ok(post_init.cluster_info.my_contact_info().into()))
    }

    fn select_active_interface(&self, meta: Self::Metadata, interface: IpAddr) -> Result<()> {
        debug!("select_active_interface received: {interface}");
        meta.with_post_init(|post_init| {
            let node = post_init.node.as_ref().ok_or_else(|| {
                jsonrpc_core::Error::invalid_params("`Node` not initialized in post_init")
            })?;

            node.switch_active_interface(interface, &post_init.cluster_info)
                .map_err(|e| {
                    jsonrpc_core::Error::invalid_params(format!(
                        "Switching failed due to error {e}"
                    ))
                })?;
            info!("Switched primary interface to {interface}");
            Ok(())
        })
    }

    fn repair_shred_from_peer(
        &self,
        meta: Self::Metadata,
        pubkey: Option<Pubkey>,
        slot: u64,
        shred_index: u64,
    ) -> Result<()> {
        debug!("repair_shred_from_peer request received");

        meta.with_post_init(|post_init| {
            repair_service::RepairService::request_repair_for_shred_from_peer(
                post_init.cluster_info.clone(),
                post_init.cluster_slots.clone(),
                pubkey,
                slot,
                shred_index,
                &post_init.repair_socket.clone(),
                post_init.outstanding_repair_requests.clone(),
            );
            Ok(())
        })
    }

    fn repair_whitelist(&self, meta: Self::Metadata) -> Result<AdminRpcRepairWhitelist> {
        debug!("repair_whitelist request received");

        meta.with_post_init(|post_init| {
            let whitelist: Vec<_> = post_init
                .repair_whitelist
                .read()
                .unwrap()
                .iter()
                .copied()
                .collect();
            Ok(AdminRpcRepairWhitelist { whitelist })
        })
    }

    fn set_repair_whitelist(&self, meta: Self::Metadata, whitelist: Vec<Pubkey>) -> Result<()> {
        debug!("set_repair_whitelist request received");

        let whitelist: HashSet<Pubkey> = whitelist.into_iter().collect();
        meta.with_post_init(|post_init| {
            *post_init.repair_whitelist.write().unwrap() = whitelist;
            warn!(
                "Repair whitelist set to {:?}",
                &post_init.repair_whitelist.read().unwrap()
            );
            Ok(())
        })
    }

    fn get_secondary_index_key_size(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
    ) -> Result<HashMap<RpcAccountIndex, usize>> {
        debug!("get_secondary_index_key_size rpc request received: {pubkey_str:?}");
        let index_key = verify_pubkey(&pubkey_str)?;
        meta.with_post_init(|post_init| {
            let bank = post_init.bank_forks.read().unwrap().root_bank();

            // Take ref to enabled AccountSecondaryIndexes
            let enabled_account_indexes = &bank.accounts().accounts_db.account_indexes;

            // Exit if secondary indexes are not enabled
            if enabled_account_indexes.is_empty() {
                debug!("get_secondary_index_key_size: secondary index not enabled.");
                return Ok(HashMap::new());
            };

            // Make sure the requested key is not explicitly excluded
            if !enabled_account_indexes.include_key(&index_key) {
                return Err(RpcCustomError::KeyExcludedFromSecondaryIndex {
                    index_key: index_key.to_string(),
                }
                .into());
            }

            // Grab a ref to the AccountsDbfor this Bank
            let accounts_index = &bank.accounts().accounts_db.accounts_index;

            // Find the size of the key in every index where it exists
            let found_sizes = enabled_account_indexes
                .indexes
                .iter()
                .filter_map(|index| {
                    accounts_index
                        .get_index_key_size(index, &index_key)
                        .map(|size| (rpc_account_index_from_account_index(index), size))
                })
                .collect::<HashMap<_, _>>();

            // Note: Will return an empty HashMap if no keys are found.
            if found_sizes.is_empty() {
                debug!("get_secondary_index_key_size: key not found in the secondary index.");
            }
            Ok(found_sizes)
        })
    }

    fn set_public_tpu_address(
        &self,
        meta: Self::Metadata,
        public_tpu_addr: SocketAddr,
    ) -> Result<()> {
        debug!("set_public_tpu_address rpc request received: {public_tpu_addr}");

        meta.with_post_init(|post_init| {
            post_init
                .cluster_info
                .my_contact_info()
                .tpu(Protocol::QUIC)
                .ok_or_else(|| {
                    error!(
                        "The public TPU QUIC address isn't being published. The node is likely in \
                         repair mode. See help for --restricted-repair-only-mode for more \
                         information."
                    );
                    jsonrpc_core::error::Error::internal_error()
                })?;
            post_init
                .cluster_info
                .set_tpu_quic(public_tpu_addr)
                .map_err(|err| {
                    error!("Failed to set public TPU QUIC address to {public_tpu_addr}: {err}");
                    jsonrpc_core::error::Error::internal_error()
                })?;
            let my_contact_info = post_init.cluster_info.my_contact_info();
            warn!(
                "Public TPU addresses set to {:?} (quic)",
                my_contact_info.tpu(Protocol::QUIC),
            );
            Ok(())
        })
    }

    fn set_public_tpu_forwards_address(
        &self,
        meta: Self::Metadata,
        public_tpu_forwards_addr: SocketAddr,
    ) -> Result<()> {
        debug!("set_public_tpu_forwards_address rpc request received: {public_tpu_forwards_addr}");

        meta.with_post_init(|post_init| {
            post_init
                .cluster_info
                .my_contact_info()
                .tpu_forwards(Protocol::QUIC)
                .ok_or_else(|| {
                    error!(
                        "The public TPU Forwards address isn't being published. The node is \
                         likely in repair mode. See help for --restricted-repair-only-mode for \
                         more information."
                    );
                    jsonrpc_core::error::Error::internal_error()
                })?;
            post_init
                .cluster_info
                .set_tpu_forwards_quic(public_tpu_forwards_addr)
                .map_err(|err| {
                    error!(
                        "Failed to set public TPU QUIC address to {public_tpu_forwards_addr}: \
                         {err}"
                    );
                    jsonrpc_core::error::Error::internal_error()
                })?;
            let my_contact_info = post_init.cluster_info.my_contact_info();
            warn!(
                "Public TPU Forwards address set to {:?} (quic)",
                my_contact_info.tpu_forwards(Protocol::QUIC),
            );
            Ok(())
        })
    }

    fn set_public_tvu_address(
        &self,
        meta: Self::Metadata,
        public_tvu_addr: SocketAddr,
    ) -> Result<()> {
        debug!("set_public_tvu_address rpc request received: {public_tvu_addr}");

        meta.with_post_init(|post_init| {
            post_init
                .cluster_info
                .my_contact_info()
                .tvu(Protocol::UDP)
                .ok_or_else(|| {
                    error!(
                        "The public TVU address isn't being published. The node is likely in \
                         repair mode. See help for --restricted-repair-only-mode for more \
                         information."
                    );
                    jsonrpc_core::error::Error::internal_error()
                })?;
            post_init
                .cluster_info
                .set_tvu_socket(public_tvu_addr)
                .map_err(|err| {
                    error!("Failed to set public TVU address to {public_tvu_addr}: {err}");
                    jsonrpc_core::error::Error::internal_error()
                })?;
            let my_contact_info = post_init.cluster_info.my_contact_info();
            warn!(
                "Public TVU addresses set to {:?}",
                my_contact_info.tvu(Protocol::UDP),
            );
            Ok(())
        })
    }

    fn manage_block_production(
        &self,
        meta: Self::Metadata,
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
        num_workers: NonZeroUsize,
        scheduler_pacing: SchedulerPacing,
    ) -> Result<()> {
        debug!("manage_block_production rpc request received");

        if num_workers > BankingStage::max_num_workers() {
            return Err(jsonrpc_core::error::Error::invalid_params(format!(
                "Number of workers ({}) exceeds maximum allowed ({})",
                num_workers,
                BankingStage::max_num_workers()
            )));
        }

        if transaction_struct != TransactionStructure::View {
            warn!("TransactionStructure::Sdk has no effect on block production");
        }

        meta.with_post_init(|post_init| {
            if post_init
                .banking_control_sender
                .try_send(BankingControlMsg::Internal {
                    block_production_method,
                    num_workers,
                    config: SchedulerConfig { scheduler_pacing },
                })
                .is_err()
            {
                error!("Banking stage already switching schedulers");

                return Err(jsonrpc_core::error::Error::internal_error());
            }

            Ok(())
        })
    }
}

impl AdminRpcImpl {
    fn add_authorized_voter_keypair(
        meta: AdminRpcRequestMetadata,
        authorized_voter: Keypair,
    ) -> Result<()> {
        let mut authorized_voter_keypairs = meta.authorized_voter_keypairs.write().unwrap();

        if authorized_voter_keypairs
            .iter()
            .any(|x| x.pubkey() == authorized_voter.pubkey())
        {
            Err(jsonrpc_core::error::Error::invalid_params(
                "Authorized voter already present",
            ))
        } else {
            authorized_voter_keypairs.push(Arc::new(authorized_voter));
            Ok(())
        }
    }

    fn set_identity_keypair(
        meta: AdminRpcRequestMetadata,
        identity_keypair: Keypair,
        require_tower: bool,
    ) -> Result<()> {
        meta.with_post_init(|post_init| {
            if require_tower {
                let _ = Tower::restore(meta.tower_storage.as_ref(), &identity_keypair.pubkey())
                    .map_err(|err| {
                        jsonrpc_core::error::Error::invalid_params(format!(
                            "Unable to load tower file for identity {}: {}",
                            identity_keypair.pubkey(),
                            err
                        ))
                    })?;
            }

            for (key, notifier) in &*post_init.notifies.read().unwrap() {
                if let Err(err) = notifier.update_key(&identity_keypair) {
                    error!("Error updating network layer keypair: {err} on {key:?}");
                }
            }

            solana_metrics::set_host_id(identity_keypair.pubkey().to_string());
            post_init
                .cluster_info
                .set_keypair(Arc::new(identity_keypair));
            warn!("Identity set to {}", post_init.cluster_info.id());
            Ok(())
        })
    }
}

fn rpc_account_index_from_account_index(account_index: &AccountIndex) -> RpcAccountIndex {
    match account_index {
        AccountIndex::ProgramId => RpcAccountIndex::ProgramId,
        AccountIndex::SplTokenOwner => RpcAccountIndex::SplTokenOwner,
        AccountIndex::SplTokenMint => RpcAccountIndex::SplTokenMint,
    }
}

// Start the Admin RPC interface
pub fn run(ledger_path: &Path, metadata: AdminRpcRequestMetadata) {
    let admin_rpc_path = admin_rpc_path(ledger_path);

    let event_loop = tokio::runtime::Builder::new_multi_thread()
        .thread_name("solAdminRpcEl")
        .worker_threads(3) // Three still seems like a lot, and better than the default of available core count
        .enable_all()
        .build()
        .unwrap();

    Builder::new()
        .name("solAdminRpc".to_string())
        .spawn(move || {
            let mut io = MetaIoHandler::default();
            io.extend_with(AdminRpcImpl.to_delegate());

            let validator_exit = metadata.validator_exit.clone();
            let server = ServerBuilder::with_meta_extractor(io, move |_req: &RequestContext| {
                metadata.clone()
            })
            .event_loop_executor(event_loop.handle().clone())
            .start(&format!("{}", admin_rpc_path.display()));

            match server {
                Err(err) => {
                    warn!("Unable to start admin rpc service: {err:?}");
                }
                Ok(server) => {
                    info!("started admin rpc service!");
                    let close_handle = server.close_handle();
                    validator_exit
                        .write()
                        .unwrap()
                        .register_exit(Box::new(move || {
                            close_handle.close();
                        }));

                    server.wait();
                }
            }
        })
        .unwrap();
}

fn admin_rpc_path(ledger_path: &Path) -> PathBuf {
    #[cfg(target_family = "windows")]
    {
        // More information about the wackiness of pipe names over at
        // https://docs.microsoft.com/en-us/windows/win32/ipc/pipe-names
        if let Some(ledger_filename) = ledger_path.file_name() {
            PathBuf::from(format!(
                "\\\\.\\pipe\\{}-admin.rpc",
                ledger_filename.to_string_lossy()
            ))
        } else {
            PathBuf::from("\\\\.\\pipe\\admin.rpc")
        }
    }
    #[cfg(not(target_family = "windows"))]
    {
        ledger_path.join("admin.rpc")
    }
}

// Connect to the Admin RPC interface
pub async fn connect(ledger_path: &Path) -> std::result::Result<gen_client::Client, RpcError> {
    let admin_rpc_path = admin_rpc_path(ledger_path);
    if !admin_rpc_path.exists() {
        Err(RpcError::Client(format!(
            "{} does not exist",
            admin_rpc_path.display()
        )))
    } else {
        ipc::connect::<_, gen_client::Client>(&format!("{}", admin_rpc_path.display())).await
    }
}

// Create a runtime for use by client side admin RPC interface calls
pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("solAdminRpcRt")
        .enable_all()
        // The agave-validator subcommands make few admin RPC calls and block
        // on the results so two workers is plenty
        .worker_threads(2)
        .build()
        .expect("new tokio runtime")
}

#[derive(Default, Deserialize, Clone)]
pub struct StakedNodesOverrides {
    #[serde(deserialize_with = "deserialize_pubkey_map")]
    pub staked_map_id: HashMap<Pubkey, u64>,
}

pub fn deserialize_pubkey_map<'de, D>(des: D) -> std::result::Result<HashMap<Pubkey, u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let container: HashMap<String, u64> = serde::Deserialize::deserialize(des)?;
    let mut container_typed: HashMap<Pubkey, u64> = HashMap::new();
    for (key, value) in container.iter() {
        let typed_key = Pubkey::try_from(key.as_str())
            .map_err(|_| serde::de::Error::invalid_type(serde::de::Unexpected::Map, &"PubKey"))?;
        container_typed.insert(typed_key, *value);
    }
    Ok(container_typed)
}

pub fn load_staked_nodes_overrides(
    path: &String,
) -> std::result::Result<StakedNodesOverrides, Box<dyn error::Error>> {
    debug!("Loading staked nodes overrides configuration from {path}");
    if Path::new(&path).exists() {
        let file = std::fs::File::open(path)?;
        Ok(serde_yaml::from_reader(file)?)
    } else {
        Err(format!("Staked nodes overrides provided '{path}' a non-existing file path.").into())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::Value,
        solana_account::{Account, AccountSharedData},
        solana_accounts_db::{
            accounts_db::{AccountsDbConfig, ACCOUNTS_DB_CONFIG_FOR_TESTING},
            accounts_index::AccountSecondaryIndexes,
        },
        solana_core::{
            admin_rpc_post_init::{KeyUpdaterType, KeyUpdaters},
            consensus::tower_storage::NullTowerStorage,
            validator::{Validator, ValidatorConfig, ValidatorTpuConfig},
        },
        solana_gossip::{cluster_info::ClusterInfo, node::Node},
        solana_ledger::{
            create_new_tmp_ledger,
            genesis_utils::{
                create_genesis_config, create_genesis_config_with_leader, GenesisConfigInfo,
            },
        },
        solana_net_utils::{sockets::bind_to_localhost_unique, SocketAddrSpace},
        solana_program_option::COption,
        solana_program_pack::Pack,
        solana_pubkey::Pubkey,
        solana_rpc::rpc::create_validator_exit,
        solana_runtime::{
            bank::{Bank, BankTestConfig},
            bank_forks::BankForks,
        },
        solana_system_interface::program as system_program,
        solana_tpu_client::tpu_client::DEFAULT_TPU_ENABLE_UDP,
        spl_generic_token::token,
        spl_token_2022_interface::state::{
            Account as TokenAccount, AccountState as TokenAccountState, Mint,
        },
        std::{collections::HashSet, fs::remove_dir_all, sync::atomic::AtomicBool},
        tokio::sync::mpsc,
    };

    #[derive(Default)]
    struct TestConfig {
        account_indexes: AccountSecondaryIndexes,
    }

    struct RpcHandler {
        io: MetaIoHandler<AdminRpcRequestMetadata>,
        meta: AdminRpcRequestMetadata,
        bank_forks: Arc<RwLock<BankForks>>,
    }

    impl RpcHandler {
        fn _start() -> Self {
            Self::start_with_config(TestConfig::default())
        }

        fn start_with_config(config: TestConfig) -> Self {
            let keypair = Arc::new(Keypair::new());
            let cluster_info = Arc::new(ClusterInfo::new(
                ContactInfo::new(
                    keypair.pubkey(),
                    solana_time_utils::timestamp(), // wallclock
                    0u16,                           // shred_version
                ),
                keypair,
                SocketAddrSpace::Unspecified,
            ));
            let exit = Arc::new(AtomicBool::new(false));
            let validator_exit = create_validator_exit(exit);
            let (bank_forks, vote_keypair) = new_bank_forks_with_config(BankTestConfig {
                accounts_db_config: AccountsDbConfig {
                    account_indexes: Some(config.account_indexes),
                    ..ACCOUNTS_DB_CONFIG_FOR_TESTING
                },
            });
            let vote_account = vote_keypair.pubkey();
            let start_progress = Arc::new(RwLock::new(ValidatorStartProgress::default()));
            let repair_whitelist = Arc::new(RwLock::new(HashSet::new()));
            let meta = AdminRpcRequestMetadata {
                rpc_addr: None,
                start_time: SystemTime::now(),
                start_progress,
                validator_exit,
                validator_exit_backpressure: HashMap::default(),
                authorized_voter_keypairs: Arc::new(RwLock::new(vec![vote_keypair])),
                tower_storage: Arc::new(NullTowerStorage {}),
                post_init: Arc::new(RwLock::new(Some(AdminRpcRequestMetadataPostInit {
                    cluster_info,
                    bank_forks: bank_forks.clone(),
                    vote_account,
                    repair_whitelist,
                    notifies: Arc::new(RwLock::new(KeyUpdaters::default())),
                    repair_socket: Arc::new(bind_to_localhost_unique().expect("should bind")),
                    outstanding_repair_requests: Arc::<
                        RwLock<repair_service::OutstandingShredRepairs>,
                    >::default(),
                    cluster_slots: Arc::new(
                        solana_core::cluster_slots_service::cluster_slots::ClusterSlots::default_for_tests(),
                    ),
                    node: None,
                    banking_control_sender: mpsc::channel(1).0,
                }))),
                staked_nodes_overrides: Arc::new(RwLock::new(HashMap::new())),
                rpc_to_plugin_manager_sender: None,
            };
            let mut io = MetaIoHandler::default();
            io.extend_with(AdminRpcImpl.to_delegate());

            Self {
                io,
                meta,
                bank_forks,
            }
        }

        fn root_bank(&self) -> Arc<Bank> {
            self.bank_forks.read().unwrap().root_bank()
        }
    }

    fn new_bank_forks_with_config(
        config: BankTestConfig,
    ) -> (Arc<RwLock<BankForks>>, Arc<Keypair>) {
        let GenesisConfigInfo {
            genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config(1_000_000_000);

        let bank = Bank::new_with_config_for_tests(&genesis_config, config);
        (BankForks::new_rw_arc(bank), Arc::new(voting_keypair))
    }

    #[test]
    fn test_secondary_index_key_sizes() {
        for secondary_index_enabled in [true, false] {
            let account_indexes = if secondary_index_enabled {
                AccountSecondaryIndexes {
                    keys: None,
                    indexes: HashSet::from([
                        AccountIndex::ProgramId,
                        AccountIndex::SplTokenMint,
                        AccountIndex::SplTokenOwner,
                    ]),
                }
            } else {
                AccountSecondaryIndexes::default()
            };

            // RPC & Bank Setup
            let rpc = RpcHandler::start_with_config(TestConfig { account_indexes });

            let bank = rpc.root_bank();
            let RpcHandler { io, meta, .. } = rpc;

            // Pubkeys
            let token_account1_pubkey = Pubkey::new_unique();
            let token_account2_pubkey = Pubkey::new_unique();
            let token_account3_pubkey = Pubkey::new_unique();
            let mint1_pubkey = Pubkey::new_unique();
            let mint2_pubkey = Pubkey::new_unique();
            let wallet1_pubkey = Pubkey::new_unique();
            let wallet2_pubkey = Pubkey::new_unique();
            let non_existent_pubkey = Pubkey::new_unique();
            let delegate = Pubkey::new_unique();

            let mut num_default_spl_token_program_accounts = 0;
            let mut num_default_system_program_accounts = 0;

            if !secondary_index_enabled {
                // Test first with no accounts added & no secondary indexes enabled:
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{token_account1_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert!(sizes.is_empty());
            } else {
                // Count SPL Token Program Default Accounts
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{}"]}}"#,
                    token::id(),
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                num_default_spl_token_program_accounts =
                    *sizes.get(&RpcAccountIndex::ProgramId).unwrap();
                // Count System Program Default Accounts
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{}"]}}"#,
                    system_program::id(),
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                num_default_system_program_accounts =
                    *sizes.get(&RpcAccountIndex::ProgramId).unwrap();
            }

            // Add 2 basic wallet accounts
            let wallet1_account = AccountSharedData::from(Account {
                lamports: 11111111,
                owner: system_program::id(),
                ..Account::default()
            });
            bank.store_account(&wallet1_pubkey, &wallet1_account);
            let wallet2_account = AccountSharedData::from(Account {
                lamports: 11111111,
                owner: system_program::id(),
                ..Account::default()
            });
            bank.store_account(&wallet2_pubkey, &wallet2_account);

            // Add a token account
            let mut account1_data = vec![0; TokenAccount::get_packed_len()];
            let token_account1 = TokenAccount {
                mint: mint1_pubkey,
                owner: wallet1_pubkey,
                delegate: COption::Some(delegate),
                amount: 420,
                state: TokenAccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 30,
                close_authority: COption::Some(wallet1_pubkey),
            };
            TokenAccount::pack(token_account1, &mut account1_data).unwrap();
            let token_account1 = AccountSharedData::from(Account {
                lamports: 111,
                data: account1_data.to_vec(),
                owner: token::id(),
                ..Account::default()
            });
            bank.store_account(&token_account1_pubkey, &token_account1);

            // Add the mint
            let mut mint1_data = vec![0; Mint::get_packed_len()];
            let mint1_state = Mint {
                mint_authority: COption::Some(wallet1_pubkey),
                supply: 500,
                decimals: 2,
                is_initialized: true,
                freeze_authority: COption::Some(wallet1_pubkey),
            };
            Mint::pack(mint1_state, &mut mint1_data).unwrap();
            let mint_account1 = AccountSharedData::from(Account {
                lamports: 222,
                data: mint1_data.to_vec(),
                owner: token::id(),
                ..Account::default()
            });
            bank.store_account(&mint1_pubkey, &mint_account1);

            // Add another token account with the different owner, but same delegate, and mint
            let mut account2_data = vec![0; TokenAccount::get_packed_len()];
            let token_account2 = TokenAccount {
                mint: mint1_pubkey,
                owner: wallet2_pubkey,
                delegate: COption::Some(delegate),
                amount: 420,
                state: TokenAccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 30,
                close_authority: COption::Some(wallet2_pubkey),
            };
            TokenAccount::pack(token_account2, &mut account2_data).unwrap();
            let token_account2 = AccountSharedData::from(Account {
                lamports: 333,
                data: account2_data.to_vec(),
                owner: token::id(),
                ..Account::default()
            });
            bank.store_account(&token_account2_pubkey, &token_account2);

            // Add another token account with the same owner and delegate but different mint
            let mut account3_data = vec![0; TokenAccount::get_packed_len()];
            let token_account3 = TokenAccount {
                mint: mint2_pubkey,
                owner: wallet2_pubkey,
                delegate: COption::Some(delegate),
                amount: 42,
                state: TokenAccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 30,
                close_authority: COption::Some(wallet2_pubkey),
            };
            TokenAccount::pack(token_account3, &mut account3_data).unwrap();
            let token_account3 = AccountSharedData::from(Account {
                lamports: 444,
                data: account3_data.to_vec(),
                owner: token::id(),
                ..Account::default()
            });
            bank.store_account(&token_account3_pubkey, &token_account3);

            // Add the new mint
            let mut mint2_data = vec![0; Mint::get_packed_len()];
            let mint2_state = Mint {
                mint_authority: COption::Some(wallet2_pubkey),
                supply: 200,
                decimals: 3,
                is_initialized: true,
                freeze_authority: COption::Some(wallet2_pubkey),
            };
            Mint::pack(mint2_state, &mut mint2_data).unwrap();
            let mint_account2 = AccountSharedData::from(Account {
                lamports: 555,
                data: mint2_data.to_vec(),
                owner: token::id(),
                ..Account::default()
            });
            bank.store_account(&mint2_pubkey, &mint_account2);

            // Accounts should now look like the following:
            //
            //                   -----system_program------
            //                  /                         \
            //                 /-(owns)                    \-(owns)
            //                /                             \
            //             wallet1                   ---wallet2---
            //               /                      /             \
            //              /-(SPL::owns)          /-(SPL::owns)   \-(SPL::owns)
            //             /                      /                 \
            //      token_account1         token_account2       token_account3
            //            \                     /                   /
            //             \-(SPL::mint)       /-(SPL::mint)       /-(SPL::mint)
            //              \                 /                   /
            //               --mint_account1--               mint_account2

            if secondary_index_enabled {
                // ----------- Test for a non-existent key -----------
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{non_existent_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert!(sizes.is_empty());
                // --------------- Test Queries ---------------
                // 1) Wallet1 - Owns 1 SPL Token
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{wallet1_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(*sizes.get(&RpcAccountIndex::SplTokenOwner).unwrap(), 1);
                // 2) Wallet2 - Owns 2 SPL Tokens
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{wallet2_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(*sizes.get(&RpcAccountIndex::SplTokenOwner).unwrap(), 2);
                // 3) Mint1 - Is in 2 SPL Accounts
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{mint1_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(*sizes.get(&RpcAccountIndex::SplTokenMint).unwrap(), 2);
                // 4) Mint2 - Is in 1 SPL Account
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{mint2_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(*sizes.get(&RpcAccountIndex::SplTokenMint).unwrap(), 1);
                // 5) SPL Token Program Owns 6 Accounts - 1 Default, 5 created above.
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{}"]}}"#,
                    token::id(),
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(
                    *sizes.get(&RpcAccountIndex::ProgramId).unwrap(),
                    (num_default_spl_token_program_accounts + 5)
                );
                // 5) System Program Owns 4 Accounts + 2 Default, 2 created above.
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{}"]}}"#,
                    system_program::id(),
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert_eq!(sizes.len(), 1);
                assert_eq!(
                    *sizes.get(&RpcAccountIndex::ProgramId).unwrap(),
                    (num_default_system_program_accounts + 2)
                );
            } else {
                // ------------ Secondary Indexes Disabled ------------
                let req = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"getSecondaryIndexKeySize","params":["{token_account2_pubkey}"]}}"#,
                );
                let res = io.handle_request_sync(&req, meta.clone());
                let result: Value = serde_json::from_str(&res.expect("actual response"))
                    .expect("actual response deserialization");
                let sizes: HashMap<RpcAccountIndex, usize> =
                    serde_json::from_value(result["result"].clone()).unwrap();
                assert!(sizes.is_empty());
            }
        }
    }

    // This test checks that the rpc call to `set_identity` works a expected with
    // Bank but without validator.
    #[test]
    fn test_set_identity() {
        let rpc = RpcHandler::start_with_config(TestConfig::default());

        let RpcHandler { io, meta, .. } = rpc;

        let expected_validator_id = Keypair::new();
        let validator_id_bytes = format!("{:?}", expected_validator_id.to_bytes());

        let set_id_request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"setIdentityFromBytes","params":[{validator_id_bytes}, false]}}"#,
        );
        let response = io.handle_request_sync(&set_id_request, meta.clone());
        let actual_parsed_response: Value =
            serde_json::from_str(&response.expect("actual response"))
                .expect("actual response deserialization");

        let expected_parsed_response: Value = serde_json::from_str(
            r#"{
                "id": 1,
                "jsonrpc": "2.0",
                "result": null
            }"#,
        )
        .expect("Failed to parse expected response");
        assert_eq!(actual_parsed_response, expected_parsed_response);

        let contact_info_request =
            r#"{"jsonrpc":"2.0","id":1,"method":"contactInfo","params":[]}"#.to_string();
        let response = io.handle_request_sync(&contact_info_request, meta.clone());
        let parsed_response: Value = serde_json::from_str(&response.expect("actual response"))
            .expect("actual response deserialization");
        let actual_validator_id = parsed_response["result"]["id"]
            .as_str()
            .expect("Expected a string");
        assert_eq!(
            actual_validator_id,
            expected_validator_id.pubkey().to_string()
        );
    }

    struct TestValidatorWithAdminRpc {
        meta: AdminRpcRequestMetadata,
        io: MetaIoHandler<AdminRpcRequestMetadata>,
        validator_ledger_path: PathBuf,
    }

    impl TestValidatorWithAdminRpc {
        fn new() -> Self {
            let leader_keypair = Keypair::new();
            let leader_node = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());

            let validator_keypair = Keypair::new();
            let validator_node = Node::new_localhost_with_pubkey(&validator_keypair.pubkey());
            let genesis_config =
                create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1000)
                    .genesis_config;
            let (validator_ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);

            let voting_keypair = Arc::new(Keypair::new());
            let voting_pubkey = voting_keypair.pubkey();
            let authorized_voter_keypairs = Arc::new(RwLock::new(vec![voting_keypair]));
            let validator_config = ValidatorConfig {
                rpc_addrs: Some((
                    validator_node.info.rpc().unwrap(),
                    validator_node.info.rpc_pubsub().unwrap(),
                )),
                ..ValidatorConfig::default_for_test()
            };
            let start_progress = Arc::new(RwLock::new(ValidatorStartProgress::default()));

            let post_init = Arc::new(RwLock::new(None));
            let meta = AdminRpcRequestMetadata {
                rpc_addr: validator_config.rpc_addrs.map(|(rpc_addr, _)| rpc_addr),
                start_time: SystemTime::now(),
                start_progress: start_progress.clone(),
                validator_exit: validator_config.validator_exit.clone(),
                validator_exit_backpressure: HashMap::default(),
                authorized_voter_keypairs: authorized_voter_keypairs.clone(),
                tower_storage: Arc::new(NullTowerStorage {}),
                post_init: post_init.clone(),
                staked_nodes_overrides: Arc::new(RwLock::new(HashMap::new())),
                rpc_to_plugin_manager_sender: None,
            };

            let _validator = Validator::new(
                validator_node,
                Arc::new(validator_keypair),
                &validator_ledger_path,
                &voting_pubkey,
                authorized_voter_keypairs,
                vec![leader_node.info],
                &validator_config,
                true, // should_check_duplicate_instance
                None, // rpc_to_plugin_manager_receiver
                start_progress.clone(),
                SocketAddrSpace::Unspecified,
                ValidatorTpuConfig::new_for_tests(DEFAULT_TPU_ENABLE_UDP),
                post_init.clone(),
            )
            .expect("assume successful validator start");
            assert_eq!(
                *start_progress.read().unwrap(),
                ValidatorStartProgress::Running
            );
            let post_init = post_init.read().unwrap();

            assert!(post_init.is_some());
            let post_init = post_init.as_ref().unwrap();
            let notifies = post_init.notifies.read().unwrap();
            let updater_keys: HashSet<KeyUpdaterType> =
                notifies.into_iter().map(|(key, _)| key.clone()).collect();
            assert_eq!(
                updater_keys,
                HashSet::from_iter(vec![
                    KeyUpdaterType::Tpu,
                    KeyUpdaterType::TpuForwards,
                    KeyUpdaterType::TpuVote,
                    KeyUpdaterType::Forward,
                    KeyUpdaterType::RpcService
                ])
            );
            let mut io = MetaIoHandler::default();
            io.extend_with(AdminRpcImpl.to_delegate());
            Self {
                meta,
                io,
                validator_ledger_path,
            }
        }

        fn handle_request(&self, request: &str) -> Option<String> {
            self.io.handle_request_sync(request, self.meta.clone())
        }
    }

    impl Drop for TestValidatorWithAdminRpc {
        fn drop(&mut self) {
            remove_dir_all(self.validator_ledger_path.clone()).unwrap();
        }
    }

    // This test checks that `set_identity` call works with working validator and client.
    #[test]
    fn test_set_identity_with_validator() {
        let test_validator = TestValidatorWithAdminRpc::new();
        let expected_validator_id = Keypair::new();
        let validator_id_bytes = format!("{:?}", expected_validator_id.to_bytes());

        let set_id_request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"setIdentityFromBytes","params":[{validator_id_bytes}, false]}}"#,
        );
        let response = test_validator.handle_request(&set_id_request);
        let actual_parsed_response: Value =
            serde_json::from_str(&response.expect("actual response"))
                .expect("actual response deserialization");

        let expected_parsed_response: Value = serde_json::from_str(
            r#"{
                "id": 1,
                "jsonrpc": "2.0",
                "result": null
            }"#,
        )
        .expect("Failed to parse expected response");
        assert_eq!(actual_parsed_response, expected_parsed_response);

        let contact_info_request =
            r#"{"jsonrpc":"2.0","id":1,"method":"contactInfo","params":[]}"#.to_string();
        let response = test_validator.handle_request(&contact_info_request);
        let parsed_response: Value = serde_json::from_str(&response.expect("actual response"))
            .expect("actual response deserialization");
        let actual_validator_id = parsed_response["result"]["id"]
            .as_str()
            .expect("Expected a string");
        assert_eq!(
            actual_validator_id,
            expected_validator_id.pubkey().to_string()
        );

        let contact_info_request =
            r#"{"jsonrpc":"2.0","id":1,"method":"exit","params":[]}"#.to_string();
        let exit_response = test_validator.handle_request(&contact_info_request);
        let actual_parsed_response: Value =
            serde_json::from_str(&exit_response.expect("actual response"))
                .expect("actual response deserialization");
        assert_eq!(actual_parsed_response, expected_parsed_response);
    }
}
