use {
    crate::{
        accounts_update_notifier::AccountsUpdateNotifierImpl,
        block_metadata_notifier::BlockMetadataNotifierImpl,
        block_metadata_notifier_interface::BlockMetadataNotifierArc,
        deshred_transaction_notifier::DeshredTransactionNotifierImpl,
        entry_notifier::EntryNotifierImpl,
        geyser_plugin_manager::{self, GeyserPluginManagerRequest},
        slot_status_notifier::SlotStatusNotifierImpl,
        slot_status_observer::SlotStatusObserver,
        transaction_notifier::TransactionNotifierImpl,
    },
    agave_geyser_plugin_host::GeyserPluginHost,
    arc_swap::ArcSwap,
    crossbeam_channel::Receiver,
    log::*,
    solana_accounts_db::accounts_update_notifier_interface::AccountsUpdateNotifier,
    solana_ledger::{
        deshred_transaction_notifier_interface::DeshredTransactionNotifierArc,
        entry_notifier_interface::EntryNotifierArc,
    },
    solana_rpc::{
        optimistically_confirmed_bank_tracker::SlotNotification,
        slot_status_notifier::SlotStatusNotifier,
        transaction_notifier_interface::TransactionNotifierArc,
    },
    std::{
        path::{Path, PathBuf},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    },
    thiserror::Error,
};

// How long to sleep between Arc::try_unwrap attempts
pub(crate) const ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION: Duration = Duration::from_millis(5);

/// The service managing the Geyser plugin workflow.
pub struct GeyserPluginService {
    slot_status_observer: Option<SlotStatusObserver>,
    plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    transaction_notifier: Option<TransactionNotifierArc>,
    deshred_transaction_notifier: Option<DeshredTransactionNotifierArc>,
    entry_notifier: Option<EntryNotifierArc>,
    block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    slot_status_notifier: Option<SlotStatusNotifier>,
}

impl GeyserPluginService {
    /// Creates and returns the GeyserPluginService.
    /// # Arguments
    /// * `confirmed_bank_receiver` - The receiver for confirmed bank notification
    /// * `geyser_plugin_always_enabled` -- Subscribe on all types of notifications, even if
    ///   no config files are passed
    /// * `geyser_plugin_config_file` - The config file path for the plugin. The
    ///   config file controls the plugin responsible
    ///   for transporting the data to external data stores. It is defined in JSON format.
    ///   The `libpath` field should be pointed to the full path of the dynamic shared library
    ///   (.so file) to be loaded. The shared library must implement the `GeyserPlugin`
    ///   trait. And the shared library shall export a `C` function `_create_plugin` which
    ///   shall create the implementation of `GeyserPlugin` and returns to the caller.
    ///   The rest of the JSON fields' definition is up to to the concrete plugin implementation
    ///   It is usually used to configure the connection information for the external data store.
    pub fn new(
        confirmed_bank_receiver: Receiver<SlotNotification>,
        geyser_plugin_always_enabled: bool,
        geyser_plugin_config_files: &[PathBuf],
    ) -> Result<Self, GeyserPluginServiceError> {
        Self::new_with_receiver(
            confirmed_bank_receiver,
            geyser_plugin_always_enabled,
            geyser_plugin_config_files,
            None,
        )
    }

    pub fn new_with_receiver(
        confirmed_bank_receiver: Receiver<SlotNotification>,
        geyser_plugin_always_enabled: bool,
        geyser_plugin_config_files: &[PathBuf],
        rpc_to_plugin_manager_receiver_and_exit: Option<(
            Receiver<GeyserPluginManagerRequest>,
            Arc<AtomicBool>,
        )>,
    ) -> Result<Self, GeyserPluginServiceError> {
        info!("Starting GeyserPluginService from config files: {geyser_plugin_config_files:?}");
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::default())));

        for geyser_plugin_config_file in geyser_plugin_config_files {
            Self::load_plugin(&plugin_manager, geyser_plugin_config_file)?;
        }

        let account_data_notifications_enabled =
            plugin_manager.load().account_data_notifications_enabled()
                || geyser_plugin_always_enabled;
        let account_data_snapshot_notifications_enabled = plugin_manager
            .load()
            .account_data_snapshot_notifications_enabled();
        let transaction_notifications_enabled =
            plugin_manager.load().transaction_notifications_enabled()
                || geyser_plugin_always_enabled;
        let deshred_transaction_notifications_enabled = plugin_manager
            .load()
            .deshred_transaction_notifications_enabled()
            || geyser_plugin_always_enabled;
        let entry_or_block_footer_notifications_enabled =
            plugin_manager.load().entry_notifications_enabled()
                || plugin_manager.load().block_footer_notifications_enabled()
                || geyser_plugin_always_enabled;

        let accounts_update_notifier: Option<AccountsUpdateNotifier> =
            if account_data_notifications_enabled {
                let accounts_update_notifier = AccountsUpdateNotifierImpl::new(
                    plugin_manager.clone(),
                    account_data_snapshot_notifications_enabled,
                );
                Some(Arc::new(accounts_update_notifier))
            } else {
                None
            };

        let transaction_notifier: Option<TransactionNotifierArc> =
            if transaction_notifications_enabled {
                let transaction_notifier = TransactionNotifierImpl::new(plugin_manager.clone());
                Some(Arc::new(transaction_notifier))
            } else {
                None
            };

        let deshred_transaction_notifier: Option<DeshredTransactionNotifierArc> =
            if deshred_transaction_notifications_enabled {
                let deshred_transaction_notifier =
                    DeshredTransactionNotifierImpl::new(plugin_manager.clone());
                Some(Arc::new(deshred_transaction_notifier))
            } else {
                None
            };

        let entry_notifier: Option<EntryNotifierArc> =
            if entry_or_block_footer_notifications_enabled {
                let entry_notifier = EntryNotifierImpl::new(plugin_manager.clone());
                Some(Arc::new(entry_notifier))
            } else {
                None
            };

        let (slot_status_observer, block_metadata_notifier, slot_status_notifier): (
            Option<SlotStatusObserver>,
            Option<BlockMetadataNotifierArc>,
            Option<SlotStatusNotifier>,
        ) = if account_data_notifications_enabled
            || transaction_notifications_enabled
            || deshred_transaction_notifications_enabled
            || entry_or_block_footer_notifications_enabled
        {
            let slot_status_notifier = SlotStatusNotifierImpl::new(plugin_manager.clone());
            let slot_status_notifier = Arc::new(RwLock::new(slot_status_notifier));
            (
                Some(SlotStatusObserver::new(
                    confirmed_bank_receiver,
                    slot_status_notifier.clone(),
                )),
                Some(Arc::new(BlockMetadataNotifierImpl::new(
                    plugin_manager.clone(),
                ))),
                Some(slot_status_notifier),
            )
        } else {
            (None, None, None)
        };

        // Initialize plugin manager rpc handler thread if needed
        if let Some((request_receiver, exit)) = rpc_to_plugin_manager_receiver_and_exit {
            let plugin_manager = plugin_manager.clone();
            Self::start_manager_rpc_handler(plugin_manager, request_receiver, exit)
        };

        info!("Started GeyserPluginService");
        Ok(GeyserPluginService {
            slot_status_observer,
            plugin_manager,
            accounts_update_notifier,
            transaction_notifier,
            deshred_transaction_notifier,
            entry_notifier,
            block_metadata_notifier,
            slot_status_notifier,
        })
    }

    fn load_plugin(
        plugin_manager: &ArcSwap<GeyserPluginHost>,
        geyser_plugin_config_file: &Path,
    ) -> Result<(), GeyserPluginServiceError> {
        geyser_plugin_manager::load_plugin(plugin_manager, geyser_plugin_config_file)
            .map_err(|e| GeyserPluginServiceError::FailedToLoadPlugin(e.into()))?;
        Ok(())
    }

    pub fn get_accounts_update_notifier(&self) -> Option<AccountsUpdateNotifier> {
        self.accounts_update_notifier.clone()
    }

    /// Cloneable handle to the underlying plugin manager. Used by
    /// startup wiring that needs to construct notifiers (e.g. the
    /// contact info notifier) outside `GeyserPluginService::new`,
    /// because they require resources (such as `ClusterInfo`) that are
    /// not available at this point.
    pub fn plugin_manager_handle(&self) -> Arc<ArcSwap<GeyserPluginHost>> {
        self.plugin_manager.clone()
    }

    pub fn get_transaction_notifier(&self) -> Option<TransactionNotifierArc> {
        self.transaction_notifier.clone()
    }

    pub fn get_deshred_transaction_notifier(&self) -> Option<DeshredTransactionNotifierArc> {
        self.deshred_transaction_notifier.clone()
    }

    pub fn get_entry_notifier(&self) -> Option<EntryNotifierArc> {
        self.entry_notifier.clone()
    }

    pub fn get_block_metadata_notifier(&self) -> Option<BlockMetadataNotifierArc> {
        self.block_metadata_notifier.clone()
    }

    pub fn get_slot_status_notifier(&self) -> Option<SlotStatusNotifier> {
        self.slot_status_notifier.clone()
    }

    pub fn join(self) -> thread::Result<()> {
        if let Some(mut slot_status_observer) = self.slot_status_observer {
            slot_status_observer.join()?;
        }

        let empty_plugin_manager = GeyserPluginHost::default();
        let mut geyser_plugin_manager_ref =
            self.plugin_manager.swap(Arc::new(empty_plugin_manager));
        loop {
            match Arc::try_unwrap(geyser_plugin_manager_ref) {
                Ok(mut geyser_plugin_manager) => {
                    geyser_plugin_manager.unload();
                    break;
                }
                Err(geyser_plugin_manager_reference) => {
                    geyser_plugin_manager_ref = geyser_plugin_manager_reference
                }
            }
            thread::sleep(ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION);
        }

        Ok(())
    }

    fn start_manager_rpc_handler(
        plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
        request_receiver: Receiver<GeyserPluginManagerRequest>,
        exit: Arc<AtomicBool>,
    ) {
        thread::Builder::new()
            .name("SolGeyserPluginRpc".to_string())
            .spawn(move || {
                loop {
                    if let Ok(request) = request_receiver.recv_timeout(Duration::from_secs(5)) {
                        match request {
                            GeyserPluginManagerRequest::ListPlugins { response_sender } => {
                                let plugin_list =
                                    geyser_plugin_manager::list_plugins(&plugin_manager.load());
                                response_sender
                                    .send(plugin_list)
                                    .expect("Admin rpc service will be waiting for response");
                            }

                            GeyserPluginManagerRequest::ReloadPlugin {
                                ref name,
                                ref config_file,
                                response_sender,
                            } => {
                                let reload_result = geyser_plugin_manager::reload_plugin(
                                    &plugin_manager,
                                    name,
                                    config_file,
                                );
                                response_sender
                                    .send(reload_result)
                                    .expect("Admin rpc service will be waiting for response");
                            }

                            GeyserPluginManagerRequest::LoadPlugin {
                                ref config_file,
                                response_sender,
                            } => {
                                let load_result = geyser_plugin_manager::load_plugin(
                                    &plugin_manager,
                                    config_file,
                                );
                                response_sender
                                    .send(load_result)
                                    .expect("Admin rpc service will be waiting for response");
                            }

                            GeyserPluginManagerRequest::UnloadPlugin {
                                ref name,
                                response_sender,
                            } => {
                                let unload_result =
                                    geyser_plugin_manager::unload_plugin(&plugin_manager, name);
                                response_sender
                                    .send(unload_result)
                                    .expect("Admin rpc service will be waiting for response");
                            }
                        }
                    }

                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })
            .unwrap();
    }
}

#[derive(Error, Debug)]
pub enum GeyserPluginServiceError {
    #[error("Failed to load a geyser plugin")]
    FailedToLoadPlugin(#[from] Box<dyn std::error::Error>),
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::geyser_plugin_manager::{TESTPLUGIN_CONFIG, TESTPLUGIN2_CONFIG},
        agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, SlotStatus},
        crossbeam_channel::{Sender, unbounded},
        jsonrpc_core::Result as JsonRpcResult,
        tokio::sync::oneshot,
    };

    #[derive(Debug)]
    struct BankStatusPlugin {
        updates: Sender<(u64, Option<u64>, SlotStatus, Option<u64>)>,
    }

    impl GeyserPlugin for BankStatusPlugin {
        fn name(&self) -> &'static str {
            "bank-status-test"
        }

        fn update_bank_status(
            &self,
            slot: u64,
            parent: Option<u64>,
            status: &SlotStatus,
            bank_id: u64,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            self.updates
                .send((slot, parent, status.clone(), Some(bank_id)))
                .unwrap();
            Ok(())
        }

        fn update_slot_status(
            &self,
            slot: u64,
            parent: Option<u64>,
            status: &SlotStatus,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            self.updates
                .send((slot, parent, status.clone(), None))
                .unwrap();
            Ok(())
        }
    }

    #[test]
    fn test_bank_notifications_without_rpc() {
        check_bank_notifications_without_rpc(true);
    }

    #[test]
    fn test_bank_notifications_without_rpc_or_transaction_history() {
        check_bank_notifications_without_rpc(false);
    }

    fn check_bank_notifications_without_rpc(transaction_history_enabled: bool) {
        use {
            agave_geyser_plugin_host::LoadedGeyserPlugin,
            crossbeam_channel::RecvTimeoutError,
            solana_ledger::genesis_utils::create_genesis_config,
            solana_rpc::{
                optimistically_confirmed_bank_tracker::{
                    BankNotification, OptimisticallyConfirmedBank,
                    OptimisticallyConfirmedBankTracker,
                },
                rpc_completed_slots_service::RpcCompletedSlotsService,
            },
            solana_runtime::{
                bank::{Bank, SlotLeader},
                bank_forks::BankForks,
                dependency_tracker::DependencyTracker,
            },
        };

        let (slot_sender, slot_receiver) = unbounded();
        let service = GeyserPluginService::new(slot_receiver, true, &[]).unwrap();
        let (updates_sender, updates_receiver) = unbounded();
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();
        // SAFETY: The plugin lives in this executable and starts no background work.
        let plugin = unsafe {
            LoadedGeyserPlugin::new(
                library.into(),
                Box::new(BankStatusPlugin {
                    updates: updates_sender,
                }),
                None,
            )
        };
        service
            .plugin_manager_handle()
            .store(Arc::new(GeyserPluginHost::from_plugins(vec![Arc::new(
                plugin,
            )])));

        let genesis_config = create_genesis_config(100).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let bank = Bank::new_from_parent(
            bank_forks.read().unwrap().root_bank(),
            SlotLeader::default(),
            1,
        );
        bank.freeze();
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(bank)
            .clone_without_scheduler();
        let bank_id = u64::from(bank.bank_id());
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let dependency_tracker =
            transaction_history_enabled.then(|| Arc::new(DependencyTracker::default()));
        let exit = Arc::new(AtomicBool::new(false));
        let (bank_sender, bank_receiver) = unbounded();
        let tracker = OptimisticallyConfirmedBankTracker::new(
            bank_receiver,
            exit.clone(),
            bank_forks,
            optimistically_confirmed_bank,
            None,
            Some(Arc::new(RwLock::new(vec![slot_sender]))),
            None,
            dependency_tracker.clone(),
        );
        let (completed_sender, completed_receiver) = unbounded();
        let completed_service = RpcCompletedSlotsService::spawn(
            completed_receiver,
            None,
            service.get_slot_status_notifier(),
            exit,
        );

        // The bank status must wait for its transaction callbacks, even without RPC.
        let work = dependency_tracker
            .as_ref()
            .map(|tracker| tracker.declare_work());
        bank_sender
            .send((BankNotification::Frozen(bank.clone()), work))
            .unwrap();
        if let Some(tracker) = dependency_tracker {
            let before_transactions = updates_receiver.recv_timeout(Duration::from_millis(100));
            // Release the dependency before asserting so a failed assertion cannot strand the worker.
            tracker.mark_this_and_all_previous_work_processed(work.unwrap());
            assert!(matches!(
                before_transactions,
                Err(RecvTimeoutError::Timeout)
            ));
        }
        let timeout = Duration::from_secs(5);
        assert_eq!(
            updates_receiver.recv_timeout(timeout).unwrap(),
            (1, Some(0), SlotStatus::Processed, Some(bank_id))
        );
        bank_sender
            .send((
                BankNotification::OptimisticallyConfirmed(1, bank.hash()),
                work,
            ))
            .unwrap();
        assert_eq!(
            updates_receiver.recv_timeout(timeout).unwrap(),
            (1, None, SlotStatus::Confirmed, Some(bank_id))
        );
        bank_sender
            .send((
                BankNotification::NewRootedChain(vec![(1, bank.bank_id())], 0),
                work,
            ))
            .unwrap();
        assert_eq!(
            updates_receiver.recv_timeout(timeout).unwrap(),
            (1, Some(0), SlotStatus::Rooted, Some(bank_id))
        );
        completed_sender.send(vec![1]).unwrap();
        assert_eq!(
            updates_receiver.recv_timeout(timeout).unwrap(),
            (1, None, SlotStatus::Completed, None)
        );
        assert!(updates_receiver.try_recv().is_err());

        // Dropping the producers must also let the no-RPC pipeline shut down.
        drop(bank_sender);
        drop(completed_sender);
        let (done_sender, done_receiver) = unbounded();
        let join = thread::spawn(move || {
            tracker.join().unwrap();
            completed_service.join().unwrap();
            service.join().unwrap();
            done_sender.send(()).unwrap();
        });
        done_receiver.recv_timeout(timeout).unwrap();
        join.join().unwrap();
    }

    fn request<T>(
        sender: &Sender<GeyserPluginManagerRequest>,
        make_request: impl FnOnce(oneshot::Sender<JsonRpcResult<T>>) -> GeyserPluginManagerRequest,
    ) -> JsonRpcResult<T> {
        let (response_sender, response) = oneshot::channel();
        sender.send(make_request(response_sender)).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), response)
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    #[test]
    fn test_service_dispatches_admin_requests_and_unloads_on_shutdown() {
        let (slot_sender, slot_receiver) = unbounded();
        let (sender, receiver) = unbounded();
        let exit = Arc::new(AtomicBool::new(false));
        let service = GeyserPluginService::new_with_receiver(
            slot_receiver,
            true,
            &[PathBuf::from(TESTPLUGIN_CONFIG)],
            Some((receiver, Arc::clone(&exit))),
        )
        .unwrap();
        let manager = service.plugin_manager_handle();
        assert!(service.get_accounts_update_notifier().is_some());
        assert!(service.get_transaction_notifier().is_some());
        assert!(service.get_deshred_transaction_notifier().is_some());
        assert!(service.get_entry_notifier().is_some());
        assert!(service.get_block_metadata_notifier().is_some());
        assert!(service.get_slot_status_notifier().is_some());

        let names = request(&sender, |response_sender| {
            GeyserPluginManagerRequest::ListPlugins { response_sender }
        })
        .unwrap();
        assert_eq!(names, ["dummy"]);
        request(&sender, |response_sender| {
            GeyserPluginManagerRequest::UnloadPlugin {
                name: "dummy".to_owned(),
                response_sender,
            }
        })
        .unwrap();
        assert!(manager.load().plugins().next().is_none());
        let name = request(&sender, |response_sender| {
            GeyserPluginManagerRequest::LoadPlugin {
                config_file: TESTPLUGIN2_CONFIG.to_owned(),
                response_sender,
            }
        })
        .unwrap();
        assert_eq!(name, "another_dummy");
        request(&sender, |response_sender| {
            GeyserPluginManagerRequest::ReloadPlugin {
                name,
                config_file: TESTPLUGIN_CONFIG.to_owned(),
                response_sender,
            }
        })
        .unwrap();
        assert_eq!(manager.load().plugins().next().unwrap().name(), "dummy");

        exit.store(true, Ordering::Relaxed);
        drop(sender);
        drop(slot_sender);
        service.join().unwrap();
        assert!(manager.load().plugins().next().is_none());
    }

    #[test]
    fn test_service_without_plugins_disables_notifiers() {
        let (_sender, receiver) = unbounded();
        let service = GeyserPluginService::new(receiver, false, &[]).unwrap();
        assert!(service.get_accounts_update_notifier().is_none());
        assert!(service.get_transaction_notifier().is_none());
        assert!(service.get_deshred_transaction_notifier().is_none());
        assert!(service.get_entry_notifier().is_none());
        assert!(service.get_block_metadata_notifier().is_none());
        assert!(service.get_slot_status_notifier().is_none());
        service.join().unwrap();
    }
}
