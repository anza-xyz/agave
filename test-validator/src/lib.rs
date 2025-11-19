#![allow(clippy::arithmetic_side_effects)]
use {
    agave_feature_set::{
        alpenglow, increase_cpi_account_info_limit, raise_cpi_nesting_limit_to_8, FeatureSet,
        FEATURE_NAMES,
    },
    agave_snapshots::{
        paths::BANK_SNAPSHOTS_DIR, snapshot_config::SnapshotConfig, SnapshotInterval,
    },
    base64::{prelude::BASE64_STANDARD, Engine},
    crossbeam_channel::Receiver,
    log::*,
    solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
    solana_accounts_db::{
        accounts_db::AccountsDbConfig, accounts_index::AccountsIndexConfig,
        utils::create_accounts_run_and_snapshot_dirs,
    },
    solana_cli_output::CliAccount,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_commitment_config::CommitmentConfig,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_core::{
        admin_rpc_post_init::AdminRpcRequestMetadataPostInit,
        consensus::tower_storage::TowerStorage,
        validator::{Validator, ValidatorConfig, ValidatorStartProgress, ValidatorTpuConfig},
    },
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::FeeRateGovernor,
    solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    solana_geyser_plugin_manager::{
        geyser_plugin_manager::GeyserPluginManager, GeyserPluginManagerRequest,
    },
    solana_gossip::{
        cluster_info::{ClusterInfo, NodeConfig},
        contact_info::Protocol,
        node::Node,
    },
    solana_inflation::Inflation,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::{read_keypair_file, write_keypair_file, Keypair},
    solana_ledger::{
        blockstore::create_new_ledger, blockstore_options::LedgerColumnOptions,
        create_new_tmp_ledger,
    },
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_message::Message,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::{
        find_available_ports_in_range, multihomed_sockets::BindIpAddrs, PortRange, SocketAddrSpace,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_rpc::{rpc::JsonRpcConfig, rpc_pubsub_service::PubSubConfig},
    solana_rpc_client::{nonblocking, rpc_client::RpcClient},
    solana_rpc_client_api::request::MAX_MULTIPLE_ACCOUNTS,
    solana_runtime::{
        bank_forks::BankForks,
        genesis_utils::{self, create_genesis_config_with_leader_ex_no_features},
        runtime_config::RuntimeConfig,
    },
    solana_sdk_ids::address_lookup_table,
    solana_signer::Signer,
    solana_streamer::quic::DEFAULT_QUIC_ENDPOINTS,
    solana_tpu_client::tpu_client::DEFAULT_TPU_ENABLE_UDP,
    solana_transaction::Transaction,
    solana_validator_exit::Exit,
    std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        fmt::Display,
        fs::{self, remove_dir_all, File},
        io::Read,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::{NonZero, NonZeroU64},
        path::{Path, PathBuf},
        str::FromStr,
        sync::{Arc, RwLock},
        time::Duration,
    },
    tokio::time::sleep,
};

#[derive(Clone)]
pub struct AccountInfo<'a> {
    pub address: Option<Pubkey>,
    pub filename: &'a str,
}

#[derive(Clone)]
pub struct UpgradeableProgramInfo {
    pub program_id: Pubkey,
    pub loader: Pubkey,
    pub upgrade_authority: Pubkey,
    pub program_path: PathBuf,
}

#[derive(Debug)]
pub struct TestValidatorNodeConfig {
    gossip_addr: SocketAddr,
    port_range: PortRange,
    bind_ip_addr: IpAddr,
}

impl Default for TestValidatorNodeConfig {
    fn default() -> Self {
        let bind_ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        #[cfg(not(debug_assertions))]
        let port_range = solana_net_utils::VALIDATOR_PORT_RANGE;
        #[cfg(debug_assertions)]
        let port_range = solana_net_utils::sockets::localhost_port_range_for_tests();
        Self {
            gossip_addr: SocketAddr::new(bind_ip_addr, port_range.0),
            port_range,
            bind_ip_addr,
        }
    }
}

pub struct TestValidatorGenesis {
    fee_rate_governor: FeeRateGovernor,
    ledger_path: Option<PathBuf>,
    tower_storage: Option<Arc<dyn TowerStorage>>,
    pub rent: Rent,
    rpc_config: JsonRpcConfig,
    pubsub_config: PubSubConfig,
    rpc_ports: Option<(u16, u16)>, // (JsonRpc, JsonRpcPubSub), None == random ports
    warp_slot: Option<Slot>,
    accounts: HashMap<Pubkey, AccountSharedData>,
    upgradeable_programs: Vec<UpgradeableProgramInfo>,
    ticks_per_slot: Option<u64>,
    epoch_schedule: Option<EpochSchedule>,
    inflation: Option<Inflation>,
    node_config: TestValidatorNodeConfig,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub start_progress: Arc<RwLock<ValidatorStartProgress>>,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
    pub max_ledger_shreds: Option<u64>,
    pub max_genesis_archive_unpacked_size: Option<u64>,
    pub geyser_plugin_config_files: Option<Vec<PathBuf>>,
    pub enable_scheduler_bindings: bool,
    deactivate_feature_set: HashSet<Pubkey>,
    compute_unit_limit: Option<u64>,
    pub log_messages_bytes_limit: Option<usize>,
    pub transaction_account_lock_limit: Option<usize>,
    pub tpu_enable_udp: bool,
    pub geyser_plugin_manager: Arc<RwLock<GeyserPluginManager>>,
    admin_rpc_service_post_init: Arc<RwLock<Option<AdminRpcRequestMetadataPostInit>>>,
}

impl Default for TestValidatorGenesis {
    fn default() -> Self {
        // Default to Tower consensus to ensure proper converage pre-Alpenglow.
        let deactivate_feature_set = [alpenglow::id()].into_iter().collect();
        Self {
            fee_rate_governor: FeeRateGovernor::default(),
            ledger_path: Option::<PathBuf>::default(),
            tower_storage: Option::<Arc<dyn TowerStorage>>::default(),
            rent: Rent::default(),
            rpc_config: JsonRpcConfig::default_for_test(),
            pubsub_config: PubSubConfig::default(),
            rpc_ports: Option::<(u16, u16)>::default(),
            warp_slot: Option::<Slot>::default(),
            accounts: HashMap::<Pubkey, AccountSharedData>::default(),
            upgradeable_programs: Vec::<UpgradeableProgramInfo>::default(),
            ticks_per_slot: Option::<u64>::default(),
            epoch_schedule: Option::<EpochSchedule>::default(),
            inflation: Option::<Inflation>::default(),
            node_config: TestValidatorNodeConfig::default(),
            validator_exit: Arc::<RwLock<Exit>>::default(),
            start_progress: Arc::<RwLock<ValidatorStartProgress>>::default(),
            authorized_voter_keypairs: Arc::<RwLock<Vec<Arc<Keypair>>>>::default(),
            staked_nodes_overrides: Arc::new(RwLock::new(HashMap::new())),
            max_ledger_shreds: Option::<u64>::default(),
            max_genesis_archive_unpacked_size: Option::<u64>::default(),
            geyser_plugin_config_files: Option::<Vec<PathBuf>>::default(),
            enable_scheduler_bindings: false,
            deactivate_feature_set,
            compute_unit_limit: Option::<u64>::default(),
            log_messages_bytes_limit: Option::<usize>::default(),
            transaction_account_lock_limit: Option::<usize>::default(),
            tpu_enable_udp: DEFAULT_TPU_ENABLE_UDP,
            geyser_plugin_manager: Arc::new(RwLock::new(GeyserPluginManager::default())),
            admin_rpc_service_post_init:
                Arc::<RwLock<Option<AdminRpcRequestMetadataPostInit>>>::default(),
        }
    }
}

fn try_transform_program_data(
    address: &Pubkey,
    account: &mut AccountSharedData,
) -> Result<(), String> {
    if account.owner() == &solana_sdk_ids::bpf_loader_upgradeable::id() {
        let programdata_offset = UpgradeableLoaderState::size_of_programdata_metadata();
        let programdata_meta = account.data().get(0..programdata_offset).ok_or(format!(
            "Failed to get upgradeable programdata data from {address}"
        ))?;
        // Ensure the account is a proper programdata account before
        // attempting to serialize into it.
        if let Ok(UpgradeableLoaderState::ProgramData {
            upgrade_authority_address,
            ..
        }) = bincode::deserialize::<UpgradeableLoaderState>(programdata_meta)
        {
            // Serialize new programdata metadata into the resulting account,
            // to overwrite the deployment slot to `0`.
            bincode::serialize_into(
                account.data_as_mut_slice(),
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address,
                },
            )
            .map_err(|_| format!("Failed to write to upgradeable programdata account {address}"))
        } else {
            Err(format!(
                "Failed to read upgradeable programdata account {address}"
            ))
        }
    } else {
        Err(format!("Account {address} not owned by upgradeable loader"))
    }
}

impl TestValidatorGenesis {
    /// Adds features to deactivate to a set, eliminating redundancies
    /// during `initialize_ledger`, if member of the set is not a Feature
    /// it will be silently ignored
    pub fn deactivate_features(&mut self, deactivate_list: &[Pubkey]) -> &mut Self {
        self.deactivate_feature_set.extend(deactivate_list);
        self
    }
    pub fn ledger_path<P: Into<PathBuf>>(&mut self, ledger_path: P) -> &mut Self {
        self.ledger_path = Some(ledger_path.into());
        self
    }

    pub fn tower_storage(&mut self, tower_storage: Arc<dyn TowerStorage>) -> &mut Self {
        self.tower_storage = Some(tower_storage);
        self
    }

    /// Check if a given TestValidator ledger has already been initialized
    pub fn ledger_exists(ledger_path: &Path) -> bool {
        ledger_path.join("vote-account-keypair.json").exists()
    }

    pub fn tpu_enable_udp(&mut self, tpu_enable_udp: bool) -> &mut Self {
        self.tpu_enable_udp = tpu_enable_udp;
        self
    }

    pub fn fee_rate_governor(&mut self, fee_rate_governor: FeeRateGovernor) -> &mut Self {
        self.fee_rate_governor = fee_rate_governor;
        self
    }

    pub fn ticks_per_slot(&mut self, ticks_per_slot: u64) -> &mut Self {
        self.ticks_per_slot = Some(ticks_per_slot);
        self
    }

    pub fn epoch_schedule(&mut self, epoch_schedule: EpochSchedule) -> &mut Self {
        self.epoch_schedule = Some(epoch_schedule);
        self
    }

    pub fn inflation(&mut self, inflation: Inflation) -> &mut Self {
        self.inflation = Some(inflation);
        self
    }

    pub fn rent(&mut self, rent: Rent) -> &mut Self {
        self.rent = rent;
        self
    }

    pub fn rpc_config(&mut self, rpc_config: JsonRpcConfig) -> &mut Self {
        self.rpc_config = rpc_config;
        self
    }

    pub fn pubsub_config(&mut self, pubsub_config: PubSubConfig) -> &mut Self {
        self.pubsub_config = pubsub_config;
        self
    }

    pub fn rpc_port(&mut self, rpc_port: u16) -> &mut Self {
        self.rpc_ports = Some((rpc_port, rpc_port + 1));
        self
    }

    pub fn faucet_addr(&mut self, faucet_addr: Option<SocketAddr>) -> &mut Self {
        self.rpc_config.faucet_addr = faucet_addr;
        self
    }

    pub fn warp_slot(&mut self, warp_slot: Slot) -> &mut Self {
        self.warp_slot = Some(warp_slot);
        self
    }

    pub fn gossip_host(&mut self, gossip_host: IpAddr) -> &mut Self {
        self.node_config.gossip_addr.set_ip(gossip_host);
        self
    }

    pub fn gossip_port(&mut self, gossip_port: u16) -> &mut Self {
        self.node_config.gossip_addr.set_port(gossip_port);
        self
    }

    pub fn port_range(&mut self, port_range: PortRange) -> &mut Self {
        self.node_config.port_range = port_range;
        self
    }

    pub fn bind_ip_addr(&mut self, bind_ip_addr: IpAddr) -> &mut Self {
        self.node_config.bind_ip_addr = bind_ip_addr;
        self
    }

    pub fn compute_unit_limit(&mut self, compute_unit_limit: u64) -> &mut Self {
        self.compute_unit_limit = Some(compute_unit_limit);
        self
    }

    /// Add an account to the test environment
    pub fn add_account(&mut self, address: Pubkey, account: AccountSharedData) -> &mut Self {
        self.accounts.insert(address, account);
        self
    }

    pub fn add_accounts<T>(&mut self, accounts: T) -> &mut Self
    where
        T: IntoIterator<Item = (Pubkey, AccountSharedData)>,
    {
        for (address, account) in accounts {
            self.add_account(address, account);
        }
        self
    }

    fn clone_accounts_and_transform<T, F>(
        &mut self,
        addresses: T,
        rpc_client: &RpcClient,
        skip_missing: bool,
        transform: F,
    ) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = Pubkey>,
        F: Fn(&Pubkey, Account) -> Result<AccountSharedData, String>,
    {
        let addresses: Vec<Pubkey> = addresses.into_iter().collect();
        for chunk in addresses.chunks(MAX_MULTIPLE_ACCOUNTS) {
            info!("Fetching {chunk:?} over RPC...");
            let responses = rpc_client
                .get_multiple_accounts(chunk)
                .map_err(|err| format!("Failed to fetch: {err}"))?;
            for (address, res) in chunk.iter().zip(responses) {
                if let Some(account) = res {
                    self.add_account(*address, transform(address, account)?);
                } else if skip_missing {
                    warn!("Could not find {address}, skipping.");
                } else {
                    return Err(format!("Failed to fetch {address}"));
                }
            }
        }
        Ok(self)
    }

    pub fn clone_accounts<T>(
        &mut self,
        addresses: T,
        rpc_client: &RpcClient,
        skip_missing: bool,
    ) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = Pubkey>,
    {
        self.clone_accounts_and_transform(
            addresses,
            rpc_client,
            skip_missing,
            |address, account| {
                let mut account_shared_data = AccountSharedData::from(account);
                // ignore the error
                try_transform_program_data(address, &mut account_shared_data).ok();
                Ok(account_shared_data)
            },
        )
    }

    pub fn deep_clone_address_lookup_table_accounts<T>(
        &mut self,
        addresses: T,
        rpc_client: &RpcClient,
    ) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = Pubkey>,
    {
        const LOOKUP_TABLE_META_SIZE: usize = 56;
        let addresses: Vec<Pubkey> = addresses.into_iter().collect();
        let mut alt_entries: Vec<Pubkey> = Vec::new();

        for chunk in addresses.chunks(MAX_MULTIPLE_ACCOUNTS) {
            info!("Fetching {chunk:?} over RPC...");
            let responses = rpc_client
                .get_multiple_accounts(chunk)
                .map_err(|err| format!("Failed to fetch: {err}"))?;
            for (address, res) in chunk.iter().zip(responses) {
                if let Some(account) = res {
                    if address_lookup_table::check_id(account.owner()) {
                        let raw_addresses_data = account
                            .data()
                            .get(LOOKUP_TABLE_META_SIZE..)
                            .ok_or(format!("Failed to get addresses data from {address}"))?;

                        if raw_addresses_data.len() % std::mem::size_of::<Pubkey>() != 0 {
                            return Err(format!("Invalid alt account data length for {address}"));
                        }

                        for address_slice in
                            raw_addresses_data.chunks_exact(std::mem::size_of::<Pubkey>())
                        {
                            // safe because size was checked earlier
                            let address = Pubkey::try_from(address_slice).unwrap();
                            alt_entries.push(address);
                        }
                        self.add_account(*address, AccountSharedData::from(account));
                    } else {
                        return Err(format!("Account {address} is not an address lookup table"));
                    }
                } else {
                    return Err(format!("Failed to fetch {address}"));
                }
            }
        }

        self.clone_accounts(alt_entries, rpc_client, true)
    }

    pub fn clone_programdata_accounts<T>(
        &mut self,
        addresses: T,
        rpc_client: &RpcClient,
        skip_missing: bool,
    ) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = Pubkey>,
    {
        self.clone_accounts_and_transform(
            addresses,
            rpc_client,
            skip_missing,
            |address, account| {
                let mut account_shared_data = AccountSharedData::from(account);
                try_transform_program_data(address, &mut account_shared_data)?;
                Ok(account_shared_data)
            },
        )
    }

    pub fn clone_upgradeable_programs<T>(
        &mut self,
        addresses: T,
        rpc_client: &RpcClient,
    ) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = Pubkey>,
    {
        let addresses: Vec<Pubkey> = addresses.into_iter().collect();
        self.clone_accounts(addresses.clone(), rpc_client, false)?;

        let mut programdata_addresses: HashSet<Pubkey> = HashSet::new();
        for address in addresses {
            let account = self.accounts.get(&address).unwrap();

            if let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = account.deserialize_data()
            {
                programdata_addresses.insert(programdata_address);
            } else {
                return Err(format!(
                    "Failed to read upgradeable program account {address}",
                ));
            }
        }

        self.clone_programdata_accounts(programdata_addresses, rpc_client, false)?;

        Ok(self)
    }

    pub fn clone_feature_set(&mut self, rpc_client: &RpcClient) -> Result<&mut Self, String> {
        for feature_ids in FEATURE_NAMES
            .keys()
            .cloned()
            .collect::<Vec<Pubkey>>()
            .chunks(MAX_MULTIPLE_ACCOUNTS)
        {
            rpc_client
                .get_multiple_accounts(feature_ids)
                .map_err(|err| format!("Failed to fetch: {err}"))?
                .into_iter()
                .zip(feature_ids)
                .for_each(|(maybe_account, feature_id)| {
                    if maybe_account
                        .as_ref()
                        .and_then(solana_feature_gate_interface::from_account)
                        .and_then(|feature| feature.activated_at)
                        .is_none()
                    {
                        self.deactivate_feature_set.insert(*feature_id);
                    }
                });
        }
        Ok(self)
    }

    pub fn add_accounts_from_json_files(
        &mut self,
        accounts: &[AccountInfo],
    ) -> Result<&mut Self, String> {
        for account in accounts {
            let Some(account_path) = solana_program_test::find_file(account.filename) else {
                return Err(format!("Unable to locate {}", account.filename));
            };
            let mut file = File::open(&account_path).unwrap();
            let mut account_info_raw = String::new();
            file.read_to_string(&mut account_info_raw).unwrap();

            let result: serde_json::Result<CliAccount> = serde_json::from_str(&account_info_raw);
            let account_info = match result {
                Err(err) => {
                    return Err(format!(
                        "Unable to deserialize {}: {}",
                        account_path.to_str().unwrap(),
                        err
                    ));
                }
                Ok(deserialized) => deserialized,
            };

            let address = account.address.unwrap_or_else(|| {
                Pubkey::from_str(account_info.keyed_account.pubkey.as_str()).unwrap()
            });
            let account = account_info
                .keyed_account
                .account
                .decode::<AccountSharedData>()
                .unwrap();

            self.add_account(address, account);
        }
        Ok(self)
    }

    pub fn add_accounts_from_directories<T, P>(&mut self, dirs: T) -> Result<&mut Self, String>
    where
        T: IntoIterator<Item = P>,
        P: AsRef<Path> + Display,
    {
        let mut json_files: HashSet<String> = HashSet::new();
        for dir in dirs {
            let matched_files = match fs::read_dir(&dir) {
                Ok(dir) => dir,
                Err(e) => return Err(format!("Cannot read directory {}: {}", &dir, e)),
            }
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension() == Some(OsStr::new("json")))
            .map(|path| String::from(path.to_string_lossy()));

            json_files.extend(matched_files);
        }

        debug!("account files found: {json_files:?}");

        let accounts: Vec<_> = json_files
            .iter()
            .map(|filename| AccountInfo {
                address: None,
                filename,
            })
            .collect();

        self.add_accounts_from_json_files(&accounts)?;

        Ok(self)
    }

    /// Add an account to the test environment with the account data in the provided `filename`
    pub fn add_account_with_file_data(
        &mut self,
        address: Pubkey,
        lamports: u64,
        owner: Pubkey,
        filename: &str,
    ) -> &mut Self {
        self.add_account(
            address,
            AccountSharedData::from(Account {
                lamports,
                data: solana_program_test::read_file(
                    solana_program_test::find_file(filename).unwrap_or_else(|| {
                        panic!("Unable to locate {filename}");
                    }),
                ),
                owner,
                executable: false,
                rent_epoch: 0,
            }),
        )
    }

    /// Add an account to the test environment with the account data in the provided as a base 64
    /// string
    pub fn add_account_with_base64_data(
        &mut self,
        address: Pubkey,
        lamports: u64,
        owner: Pubkey,
        data_base64: &str,
    ) -> &mut Self {
        self.add_account(
            address,
            AccountSharedData::from(Account {
                lamports,
                data: BASE64_STANDARD
                    .decode(data_base64)
                    .unwrap_or_else(|err| panic!("Failed to base64 decode: {err}")),
                owner,
                executable: false,
                rent_epoch: 0,
            }),
        )
    }

    /// Add a SBF program to the test environment.
    ///
    /// `program_name` will also used to locate the SBF shared object in the current or fixtures
    /// directory.
    pub fn add_program(&mut self, program_name: &str, program_id: Pubkey) -> &mut Self {
        let program_path = solana_program_test::find_file(&format!("{program_name}.so"))
            .unwrap_or_else(|| panic!("Unable to locate program {program_name}"));

        self.upgradeable_programs.push(UpgradeableProgramInfo {
            program_id,
            loader: solana_sdk_ids::bpf_loader_upgradeable::id(),
            upgrade_authority: Pubkey::default(),
            program_path,
        });
        self
    }

    /// Add a list of upgradeable programs to the test environment.
    pub fn add_upgradeable_programs_with_path(
        &mut self,
        programs: &[UpgradeableProgramInfo],
    ) -> &mut Self {
        for program in programs {
            self.upgradeable_programs.push(program.clone());
        }
        self
    }

    /// Start a test validator with the address of the mint account that will receive tokens
    /// created at genesis.
    ///
    pub fn start_with_mint_address(
        &self,
        mint_address: Pubkey,
        socket_addr_space: SocketAddrSpace,
    ) -> Result<TestValidator, Box<dyn std::error::Error>> {
        self.start_with_mint_address_and_geyser_plugin_rpc(mint_address, socket_addr_space, None)
    }

    /// Start a test validator with the address of the mint account that will receive tokens
    /// created at genesis. Augments admin rpc service with dynamic geyser plugin manager if
    /// the geyser plugin service is enabled at startup.
    ///
    pub fn start_with_mint_address_and_geyser_plugin_rpc(
        &self,
        mint_address: Pubkey,
        socket_addr_space: SocketAddrSpace,
        rpc_to_plugin_manager_receiver: Option<Receiver<GeyserPluginManagerRequest>>,
    ) -> Result<TestValidator, Box<dyn std::error::Error>> {
        TestValidator::start(
            mint_address,
            self,
            socket_addr_space,
            rpc_to_plugin_manager_receiver,
        )
        .inspect(|test_validator| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(test_validator.wait_for_nonzero_fees());
        })
    }

    /// Start a test validator
    ///
    /// Returns a new `TestValidator` as well as the keypair for the mint account that will receive tokens
    /// created at genesis.
    ///
    /// This function panics on initialization failure.
    pub fn start(&self) -> (TestValidator, Keypair) {
        self.start_with_socket_addr_space(SocketAddrSpace::new(/*allow_private_addr=*/ true))
    }

    /// Start a test validator with the given `SocketAddrSpace`
    ///
    /// Returns a new `TestValidator` as well as the keypair for the mint account that will receive tokens
    /// created at genesis.
    ///
    /// This function panics on initialization failure.
    pub fn start_with_socket_addr_space(
        &self,
        socket_addr_space: SocketAddrSpace,
    ) -> (TestValidator, Keypair) {
        let mint_keypair = Keypair::new();
        self.start_with_mint_address(mint_keypair.pubkey(), socket_addr_space)
            .inspect(|test_validator| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .unwrap();
                let upgradeable_program_ids: Vec<&Pubkey> = self
                    .upgradeable_programs
                    .iter()
                    .map(|p| &p.program_id)
                    .collect();
                runtime.block_on(test_validator.wait_for_upgradeable_programs_deployed(
                    &upgradeable_program_ids,
                    &mint_keypair,
                ));
            })
            .map(|test_validator| (test_validator, mint_keypair))
            .unwrap_or_else(|err| panic!("Test validator failed to start: {err}"))
    }

    pub async fn start_async(&self) -> (TestValidator, Keypair) {
        self.start_async_with_socket_addr_space(SocketAddrSpace::new(
            /*allow_private_addr=*/ true,
        ))
        .await
    }

    pub async fn start_async_with_socket_addr_space(
        &self,
        socket_addr_space: SocketAddrSpace,
    ) -> (TestValidator, Keypair) {
        let mint_keypair = Keypair::new();
        match TestValidator::start(mint_keypair.pubkey(), self, socket_addr_space, None) {
            Ok(test_validator) => {
                test_validator.wait_for_nonzero_fees().await;
                let upgradeable_program_ids: Vec<&Pubkey> = self
                    .upgradeable_programs
                    .iter()
                    .map(|p| &p.program_id)
                    .collect();
                test_validator
                    .wait_for_upgradeable_programs_deployed(&upgradeable_program_ids, &mint_keypair)
                    .await;
                (test_validator, mint_keypair)
            }
            Err(err) => panic!("Test validator failed to start: {err}"),
        }
    }
}

pub struct TestValidator {
    ledger_path: PathBuf,
    preserve_ledger: bool,
    rpc_pubsub_url: String,
    rpc_url: String,
    tpu: SocketAddr,
    gossip: SocketAddr,
    validator: Option<Validator>,
    vote_account_address: Pubkey,
}

impl TestValidator {
    /// Create and start a `TestValidator` with no transaction fees and minimal rent.
    /// Faucet optional.
    ///
    /// This function panics on initialization failure.
    pub fn with_no_fees(
        mint_address: Pubkey,
        faucet_addr: Option<SocketAddr>,
        socket_addr_space: SocketAddrSpace,
    ) -> Self {
        TestValidatorGenesis::default()
            .fee_rate_governor(FeeRateGovernor::new(0, 0))
            .rent(Rent {
                lamports_per_byte_year: 1,
                exemption_threshold: 1.0,
                ..Rent::default()
            })
            .faucet_addr(faucet_addr)
            .start_with_mint_address(mint_address, socket_addr_space)
            .expect("validator start failed")
    }

    /// Create a test validator using udp for TPU.
    pub fn with_no_fees_udp(
        mint_address: Pubkey,
        faucet_addr: Option<SocketAddr>,
        socket_addr_space: SocketAddrSpace,
    ) -> Self {
        TestValidatorGenesis::default()
            .tpu_enable_udp(true)
            .fee_rate_governor(FeeRateGovernor::new(0, 0))
            .rent(Rent {
                lamports_per_byte_year: 1,
                exemption_threshold: 1.0,
                ..Rent::default()
            })
            .faucet_addr(faucet_addr)
            .start_with_mint_address(mint_address, socket_addr_space)
            .expect("validator start failed")
    }

    /// Create and start a `TestValidator` with custom transaction fees and minimal rent.
    /// Faucet optional.
    ///
    /// This function panics on initialization failure.
    pub fn with_custom_fees(
        mint_address: Pubkey,
        target_lamports_per_signature: u64,
        faucet_addr: Option<SocketAddr>,
        socket_addr_space: SocketAddrSpace,
    ) -> Self {
        TestValidatorGenesis::default()
            .fee_rate_governor(FeeRateGovernor::new(target_lamports_per_signature, 0))
            .rent(Rent {
                lamports_per_byte_year: 1,
                exemption_threshold: 1.0,
                ..Rent::default()
            })
            .faucet_addr(faucet_addr)
            .start_with_mint_address(mint_address, socket_addr_space)
            .expect("validator start failed")
    }

    /// Initialize the ledger directory
    ///
    /// If `ledger_path` is `None`, a temporary ledger will be created.  Otherwise the ledger will
    /// be initialized in the provided directory if it doesn't already exist.
    ///
    /// Returns the path to the ledger directory.
    fn initialize_ledger(
        mint_address: Pubkey,
        config: &TestValidatorGenesis,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let validator_identity = Keypair::new();
        let validator_vote_account = Keypair::new();
        let validator_stake_account = Keypair::new();
        let validator_identity_lamports = 500 * LAMPORTS_PER_SOL;
        let validator_stake_lamports = 1_000_000 * LAMPORTS_PER_SOL;
        let mint_lamports = 500_000_000 * LAMPORTS_PER_SOL;

        // Only activate features which are not explicitly deactivated.
        let mut feature_set = FeatureSet::default().inactive().clone();
        for feature in &config.deactivate_feature_set {
            if feature_set.remove(feature) {
                info!("Feature for {feature:?} deactivated")
            } else {
                warn!("Feature {feature:?} set for deactivation is not a known Feature public key",)
            }
        }

        let mut accounts = config.accounts.clone();
        for (address, account) in solana_program_binaries::spl_programs(&config.rent) {
            accounts.entry(address).or_insert(account);
        }
        for (address, account) in
            solana_program_binaries::core_bpf_programs(&config.rent, |feature_id| {
                feature_set.contains(feature_id)
            })
        {
            accounts.entry(address).or_insert(account);
        }
        for upgradeable_program in &config.upgradeable_programs {
            let data = solana_program_test::read_file(&upgradeable_program.program_path);
            let (programdata_address, _) = Pubkey::find_program_address(
                &[upgradeable_program.program_id.as_ref()],
                &upgradeable_program.loader,
            );
            let mut program_data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(upgradeable_program.upgrade_authority),
            })
            .unwrap();
            program_data.extend_from_slice(&data);
            accounts.insert(
                programdata_address,
                AccountSharedData::from(Account {
                    lamports: Rent::default().minimum_balance(program_data.len()).max(1),
                    data: program_data,
                    owner: upgradeable_program.loader,
                    executable: false,
                    rent_epoch: 0,
                }),
            );

            let data = bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();
            accounts.insert(
                upgradeable_program.program_id,
                AccountSharedData::from(Account {
                    lamports: Rent::default().minimum_balance(data.len()).max(1),
                    data,
                    owner: upgradeable_program.loader,
                    executable: true,
                    rent_epoch: 0,
                }),
            );
        }

        let mut genesis_config = create_genesis_config_with_leader_ex_no_features(
            mint_lamports,
            &mint_address,
            &validator_identity.pubkey(),
            &validator_vote_account.pubkey(),
            &validator_stake_account.pubkey(),
            None,
            validator_stake_lamports,
            validator_identity_lamports,
            config.fee_rate_governor.clone(),
            config.rent.clone(),
            solana_cluster_type::ClusterType::Development,
            accounts.into_iter().collect(),
        );
        genesis_config.epoch_schedule = config
            .epoch_schedule
            .as_ref()
            .cloned()
            .unwrap_or_else(EpochSchedule::without_warmup);

        if let Some(ticks_per_slot) = config.ticks_per_slot {
            genesis_config.ticks_per_slot = ticks_per_slot;
        }

        if let Some(inflation) = config.inflation {
            genesis_config.inflation = inflation;
        }

        for feature in feature_set {
            genesis_utils::activate_feature(&mut genesis_config, feature);
        }

        let ledger_path = match &config.ledger_path {
            None => create_new_tmp_ledger!(&genesis_config).0,
            Some(ledger_path) => {
                if TestValidatorGenesis::ledger_exists(ledger_path) {
                    return Ok(ledger_path.to_path_buf());
                }

                let _ = create_new_ledger(
                    ledger_path,
                    &genesis_config,
                    config
                        .max_genesis_archive_unpacked_size
                        .unwrap_or(MAX_GENESIS_ARCHIVE_UNPACKED_SIZE),
                    LedgerColumnOptions::default(),
                )
                .map_err(|err| {
                    format!(
                        "Failed to create ledger at {}: {}",
                        ledger_path.display(),
                        err
                    )
                })?;
                ledger_path.to_path_buf()
            }
        };

        write_keypair_file(
            &validator_identity,
            ledger_path.join("validator-keypair.json").to_str().unwrap(),
        )?;

        write_keypair_file(
            &validator_stake_account,
            ledger_path
                .join("stake-account-keypair.json")
                .to_str()
                .unwrap(),
        )?;

        // `ledger_exists` should fail until the vote account keypair is written
        assert!(!TestValidatorGenesis::ledger_exists(&ledger_path));

        write_keypair_file(
            &validator_vote_account,
            ledger_path
                .join("vote-account-keypair.json")
                .to_str()
                .unwrap(),
        )?;

        Ok(ledger_path)
    }

    /// Starts a TestValidator at the provided ledger directory
    fn start(
        mint_address: Pubkey,
        config: &TestValidatorGenesis,
        socket_addr_space: SocketAddrSpace,
        rpc_to_plugin_manager_receiver: Option<Receiver<GeyserPluginManagerRequest>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let preserve_ledger = config.ledger_path.is_some();
        let ledger_path = TestValidator::initialize_ledger(mint_address, config)?;

        let validator_identity =
            read_keypair_file(ledger_path.join("validator-keypair.json").to_str().unwrap())?;
        let validator_vote_account = read_keypair_file(
            ledger_path
                .join("vote-account-keypair.json")
                .to_str()
                .unwrap(),
        )?;
        let node = {
            let bind_ip_addr = config.node_config.bind_ip_addr;
            let validator_node_config = NodeConfig {
                bind_ip_addrs: BindIpAddrs::new(vec![bind_ip_addr])?,
                gossip_port: config.node_config.gossip_addr.port(),
                port_range: config.node_config.port_range,
                advertised_ip: bind_ip_addr,
                public_tvu_addr: None,
                public_tpu_addr: None,
                public_tpu_forwards_addr: None,
                num_tvu_receive_sockets: NonZero::new(1).unwrap(),
                num_tvu_retransmit_sockets: NonZero::new(1).unwrap(),
                num_quic_endpoints: NonZero::new(DEFAULT_QUIC_ENDPOINTS)
                    .expect("Number of QUIC endpoints can not be zero"),
                vortexor_receiver_addr: None,
            };
            let mut node =
                Node::new_with_external_ip(&validator_identity.pubkey(), validator_node_config);
            let (rpc, rpc_pubsub) = config.rpc_ports.unwrap_or_else(|| {
                let rpc_ports: [u16; 2] =
                    find_available_ports_in_range(bind_ip_addr, config.node_config.port_range)
                        .unwrap();
                (rpc_ports[0], rpc_ports[1])
            });
            node.info.set_rpc((bind_ip_addr, rpc)).unwrap();
            node.info
                .set_rpc_pubsub((bind_ip_addr, rpc_pubsub))
                .unwrap();
            node
        };

        let vote_account_address = validator_vote_account.pubkey();
        let rpc_url = format!("http://{}", node.info.rpc().unwrap());
        let rpc_pubsub_url = format!("ws://{}/", node.info.rpc_pubsub().unwrap());
        let tpu = node.info.tpu(Protocol::UDP).unwrap();
        let gossip = node.info.gossip().unwrap();

        {
            let mut authorized_voter_keypairs = config.authorized_voter_keypairs.write().unwrap();
            if !authorized_voter_keypairs
                .iter()
                .any(|x| x.pubkey() == vote_account_address)
            {
                authorized_voter_keypairs.push(Arc::new(validator_vote_account))
            }
        }

        let accounts_db_config = AccountsDbConfig {
            index: Some(AccountsIndexConfig::default()),
            account_indexes: Some(config.rpc_config.account_indexes.clone()),
            ..AccountsDbConfig::default()
        };

        let runtime_config = RuntimeConfig {
            compute_budget: config
                .compute_unit_limit
                .map(|compute_unit_limit| ComputeBudget {
                    compute_unit_limit,
                    ..ComputeBudget::new_with_defaults(
                        !config
                            .deactivate_feature_set
                            .contains(&raise_cpi_nesting_limit_to_8::id()),
                        !config
                            .deactivate_feature_set
                            .contains(&increase_cpi_account_info_limit::id()),
                    )
                }),
            log_messages_bytes_limit: config.log_messages_bytes_limit,
            transaction_account_lock_limit: config.transaction_account_lock_limit,
        };

        let mut validator_config = ValidatorConfig {
            on_start_geyser_plugin_config_files: config.geyser_plugin_config_files.clone(),
            rpc_addrs: Some((
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    node.info.rpc().unwrap().port(),
                ),
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    node.info.rpc_pubsub().unwrap().port(),
                ),
            )),
            rpc_config: config.rpc_config.clone(),
            pubsub_config: config.pubsub_config.clone(),
            account_paths: vec![
                create_accounts_run_and_snapshot_dirs(ledger_path.join("accounts"))
                    .unwrap()
                    .0,
            ],
            run_verification: false, // Skip PoH verification of ledger on startup for speed
            snapshot_config: SnapshotConfig {
                full_snapshot_archive_interval: SnapshotInterval::Slots(
                    NonZeroU64::new(100).unwrap(),
                ),
                incremental_snapshot_archive_interval: SnapshotInterval::Disabled,
                bank_snapshots_dir: ledger_path.join(BANK_SNAPSHOTS_DIR),
                full_snapshot_archives_dir: ledger_path.to_path_buf(),
                incremental_snapshot_archives_dir: ledger_path.to_path_buf(),
                ..SnapshotConfig::default()
            },
            warp_slot: config.warp_slot,
            validator_exit: config.validator_exit.clone(),
            max_ledger_shreds: config.max_ledger_shreds,
            no_wait_for_vote_to_start_leader: true,
            staked_nodes_overrides: config.staked_nodes_overrides.clone(),
            accounts_db_config,
            runtime_config,
            enable_scheduler_bindings: config.enable_scheduler_bindings,
            ..ValidatorConfig::default_for_test()
        };
        if let Some(ref tower_storage) = config.tower_storage {
            validator_config.tower_storage = tower_storage.clone();
        }

        let validator = Some(Validator::new(
            node,
            Arc::new(validator_identity),
            &ledger_path,
            &vote_account_address,
            config.authorized_voter_keypairs.clone(),
            vec![],
            &validator_config,
            true, // should_check_duplicate_instance
            rpc_to_plugin_manager_receiver,
            config.start_progress.clone(),
            socket_addr_space,
            ValidatorTpuConfig::new_for_tests(config.tpu_enable_udp),
            config.admin_rpc_service_post_init.clone(),
        )?);

        let test_validator = TestValidator {
            ledger_path,
            preserve_ledger,
            rpc_pubsub_url,
            rpc_url,
            tpu,
            gossip,
            validator,
            vote_account_address,
        };
        Ok(test_validator)
    }

    /// This is a hack to delay until the fees are non-zero for test consistency
    /// (fees from genesis are zero until the first block with a transaction in it is completed
    ///  due to a bug in the Bank)
    async fn wait_for_nonzero_fees(&self) {
        let rpc_client = nonblocking::rpc_client::RpcClient::new_with_commitment(
            self.rpc_url.clone(),
            CommitmentConfig::processed(),
        );
        let mut message = Message::new(
            &[Instruction::new_with_bytes(
                Pubkey::new_unique(),
                &[],
                vec![AccountMeta::new(Pubkey::new_unique(), true)],
            )],
            None,
        );
        const MAX_TRIES: u64 = 10;
        let mut num_tries = 0;
        loop {
            num_tries += 1;
            if num_tries > MAX_TRIES {
                break;
            }
            println!("Waiting for fees to stabilize {num_tries:?}...");
            match rpc_client.get_latest_blockhash().await {
                Ok(blockhash) => {
                    message.recent_blockhash = blockhash;
                    match rpc_client.get_fee_for_message(&message).await {
                        Ok(fee) => {
                            if fee != 0 {
                                break;
                            }
                        }
                        Err(err) => {
                            warn!("get_fee_for_message() failed: {err:?}");
                            break;
                        }
                    }
                }
                Err(err) => {
                    warn!("get_latest_blockhash() failed: {err:?}");
                    break;
                }
            }
            sleep(Duration::from_millis(DEFAULT_MS_PER_SLOT)).await;
        }
    }

    /// programs added to genesis ain't immediately usable. Actively check "Program
    /// is not deployed" error for their availability.
    async fn wait_for_upgradeable_programs_deployed(
        &self,
        upgradeable_programs: &[&Pubkey],
        payer: &Keypair,
    ) {
        let rpc_client = nonblocking::rpc_client::RpcClient::new_with_commitment(
            self.rpc_url.clone(),
            CommitmentConfig::processed(),
        );

        let mut deployed = vec![false; upgradeable_programs.len()];
        const MAX_ATTEMPTS: u64 = 10;

        for attempt in 1..=MAX_ATTEMPTS {
            let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
            for (program_id, is_deployed) in upgradeable_programs.iter().zip(deployed.iter_mut()) {
                if *is_deployed {
                    continue;
                }

                let transaction = Transaction::new_signed_with_payer(
                    &[Instruction {
                        program_id: **program_id,
                        accounts: vec![],
                        data: vec![],
                    }],
                    Some(&payer.pubkey()),
                    &[&payer],
                    blockhash,
                );
                match rpc_client.send_transaction(&transaction).await {
                    Ok(_) => *is_deployed = true,
                    Err(e) => {
                        if format!("{e:?}").contains("Program is not deployed") {
                            debug!("{program_id:?} - not deployed");
                        } else {
                            // Assuming all other other errors could only occur *after*
                            // program is deployed for usability.
                            *is_deployed = true;
                            debug!("{program_id:?} - Unexpected error: {e:?}");
                        }
                    }
                }
            }
            if deployed.iter().all(|&deployed| deployed) {
                return;
            }

            println!("Waiting for programs to be fully deployed {attempt} ...");
            sleep(Duration::from_millis(DEFAULT_MS_PER_SLOT)).await;
        }
        panic!("Timeout waiting for program to become usable");
    }

    /// Return the validator's TPU address
    pub fn tpu(&self) -> &SocketAddr {
        &self.tpu
    }

    /// Return the validator's Gossip address
    pub fn gossip(&self) -> &SocketAddr {
        &self.gossip
    }

    /// Return the validator's JSON RPC URL
    pub fn rpc_url(&self) -> String {
        self.rpc_url.clone()
    }

    /// Return the validator's JSON RPC PubSub URL
    pub fn rpc_pubsub_url(&self) -> String {
        self.rpc_pubsub_url.clone()
    }

    /// Return the validator's vote account address
    pub fn vote_account_address(&self) -> Pubkey {
        self.vote_account_address
    }

    /// Return an RpcClient for the validator.
    pub fn get_rpc_client(&self) -> RpcClient {
        RpcClient::new_with_commitment(self.rpc_url.clone(), CommitmentConfig::processed())
    }

    /// Return a nonblocking RpcClient for the validator.
    pub fn get_async_rpc_client(&self) -> nonblocking::rpc_client::RpcClient {
        nonblocking::rpc_client::RpcClient::new_with_commitment(
            self.rpc_url.clone(),
            CommitmentConfig::processed(),
        )
    }

    pub fn join(mut self) {
        if let Some(validator) = self.validator.take() {
            validator.join();
        }
    }

    pub fn cluster_info(&self) -> Arc<ClusterInfo> {
        self.validator.as_ref().unwrap().cluster_info.clone()
    }

    pub fn bank_forks(&self) -> Arc<RwLock<BankForks>> {
        self.validator.as_ref().unwrap().bank_forks.clone()
    }

    pub fn repair_whitelist(&self) -> Arc<RwLock<HashSet<Pubkey>>> {
        Arc::new(RwLock::new(HashSet::default()))
    }
}

impl Drop for TestValidator {
    fn drop(&mut self) {
        if let Some(validator) = self.validator.take() {
            validator.close();
        }
        if !self.preserve_ledger {
            remove_dir_all(&self.ledger_path).unwrap_or_else(|err| {
                panic!(
                    "Failed to remove ledger directory {}: {}",
                    self.ledger_path.display(),
                    err
                )
            });
        }
    }
}

#[cfg(test)]
mod test {
    use {super::*, solana_feature_gate_interface::Feature};

    #[test]
    fn get_health() {
        let (test_validator, _payer) = TestValidatorGenesis::default().start();
        let rpc_client = test_validator.get_rpc_client();
        rpc_client.get_health().expect("health");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonblocking_get_health() {
        let (test_validator, _payer) = TestValidatorGenesis::default().start_async().await;
        let rpc_client = test_validator.get_async_rpc_client();
        rpc_client.get_health().await.expect("health");
    }

    #[test]
    fn test_upgradeable_program_deploayment() {
        let program_id = Pubkey::new_unique();
        let (test_validator, payer) = TestValidatorGenesis::default()
            .add_program("../programs/bpf-loader-tests/noop", program_id)
            .start();
        let rpc_client = test_validator.get_rpc_client();

        let blockhash = rpc_client.get_latest_blockhash().unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[Instruction {
                program_id,
                accounts: vec![],
                data: vec![],
            }],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        assert!(rpc_client
            .send_and_confirm_transaction(&transaction)
            .is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_nonblocking_upgradeable_program_deploayment() {
        let program_id = Pubkey::new_unique();
        let (test_validator, payer) = TestValidatorGenesis::default()
            .add_program("../programs/bpf-loader-tests/noop", program_id)
            .start_async()
            .await;
        let rpc_client = test_validator.get_async_rpc_client();

        let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[Instruction {
                program_id,
                accounts: vec![],
                data: vec![],
            }],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        assert!(rpc_client
            .send_and_confirm_transaction(&transaction)
            .await
            .is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[should_panic]
    async fn document_tokio_panic() {
        // `start()` blows up when run within tokio
        let (_test_validator, _payer) = TestValidatorGenesis::default().start();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_deactivate_features() {
        let mut control = FeatureSet::default().inactive().clone();
        let mut deactivate_features = Vec::new();
        [
            agave_feature_set::deprecate_rewards_sysvar::id(),
            agave_feature_set::disable_fees_sysvar::id(),
            alpenglow::id(),
        ]
        .into_iter()
        .for_each(|feature| {
            control.remove(&feature);
            deactivate_features.push(feature);
        });

        // Convert to `Vec` so we can get a slice.
        let control: Vec<Pubkey> = control.into_iter().collect();

        let (test_validator, _payer) = TestValidatorGenesis::default()
            .deactivate_features(&deactivate_features)
            .start_async()
            .await;

        let rpc_client = test_validator.get_async_rpc_client();

        // Our deactivated features should be inactive.
        let inactive_feature_accounts = rpc_client
            .get_multiple_accounts(&deactivate_features)
            .await
            .unwrap();
        for f in inactive_feature_accounts {
            assert!(f.is_none());
        }

        // Everything else should be active.
        for chunk in control.chunks(100) {
            let active_feature_accounts = rpc_client.get_multiple_accounts(chunk).await.unwrap();
            for f in active_feature_accounts {
                let account = f.unwrap(); // Should be `Some`.
                let feature_state: Feature = bincode::deserialize(account.data()).unwrap();
                assert!(feature_state.activated_at.is_some());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_override_feature_account() {
        let with_deactivate_flag = agave_feature_set::deprecate_rewards_sysvar::id();
        let without_deactivate_flag = agave_feature_set::disable_fees_sysvar::id();

        let owner = Pubkey::new_unique();
        let account = || AccountSharedData::new(100_000, 0, &owner);

        let (test_validator, _payer) = TestValidatorGenesis::default()
            .deactivate_features(&[with_deactivate_flag]) // Just deactivate one feature.
            .add_accounts([
                (with_deactivate_flag, account()), // But add both accounts.
                (without_deactivate_flag, account()),
            ])
            .start_async()
            .await;

        let rpc_client = test_validator.get_async_rpc_client();

        let our_accounts = rpc_client
            .get_multiple_accounts(&[with_deactivate_flag, without_deactivate_flag])
            .await
            .unwrap();

        // The first one, where we provided `--deactivate-feature`, should be
        // the account we provided.
        let overridden_account = our_accounts[0].as_ref().unwrap();
        assert_eq!(overridden_account.lamports, 100_000);
        assert_eq!(overridden_account.data.len(), 0);
        assert_eq!(overridden_account.owner, owner);

        // The second one should be a feature account.
        let feature_account = our_accounts[1].as_ref().unwrap();
        assert_eq!(feature_account.owner, solana_sdk_ids::feature::id());
        let feature_state: Feature = bincode::deserialize(feature_account.data()).unwrap();
        assert!(feature_state.activated_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_core_bpf_programs() {
        let (test_validator, _payer) = TestValidatorGenesis::default().start_async().await;

        let rpc_client = test_validator.get_async_rpc_client();

        let fetched_programs = rpc_client
            .get_multiple_accounts(&[
                solana_sdk_ids::address_lookup_table::id(),
                solana_sdk_ids::config::id(),
                solana_sdk_ids::feature::id(),
                solana_sdk_ids::stake::id(),
            ])
            .await
            .unwrap();

        // Address lookup table is a BPF program.
        let account = fetched_programs[0].as_ref().unwrap();
        assert_eq!(account.owner, solana_sdk_ids::bpf_loader_upgradeable::id());
        assert!(account.executable);

        // Config is a BPF program.
        let account = fetched_programs[1].as_ref().unwrap();
        assert_eq!(account.owner, solana_sdk_ids::bpf_loader_upgradeable::id());
        assert!(account.executable);

        // Feature Gate is a BPF program.
        let account = fetched_programs[2].as_ref().unwrap();
        assert_eq!(account.owner, solana_sdk_ids::bpf_loader_upgradeable::id());
        assert!(account.executable);

        // Stake is a BPF program.
        let account = fetched_programs[3].as_ref().unwrap();
        assert_eq!(account.owner, solana_sdk_ids::bpf_loader_upgradeable::id());
        assert!(account.executable);
    }
}
