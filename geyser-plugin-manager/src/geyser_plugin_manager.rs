//! Admin RPC operations over the loaded Geyser plugin set.
//!
//! The plugin set itself, and the loading of plugin shared libraries, live in
//! `agave-geyser-plugin-host`. What stays here is the validator-operational
//! surface: the admin RPC handlers for listing, loading, unloading and
//! reloading plugins at runtime.

#[cfg(not(test))]
use agave_geyser_plugin_host::load_plugin_from_config;
#[cfg(test)]
use agave_geyser_plugin_host::{GeyserPluginManagerError, LoadedGeyserPlugin};
use {
    agave_geyser_plugin_host::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
    arc_swap::ArcSwap,
    jsonrpc_core::{ErrorCode, Result as JsonRpcResult},
    std::{path::Path, sync::Arc},
    tokio::sync::oneshot::Sender as OneShotSender,
};

/// Admin RPC request handler
pub(crate) fn list_plugins(manager: &GeyserPluginManager) -> JsonRpcResult<Vec<String>> {
    Ok(manager
        .plugins()
        .iter()
        .map(|p| p.name().to_owned())
        .collect())
}

/// Admin RPC request handler
/// # Safety
///
/// This function loads the dynamically linked library specified in the path. The library
/// must do necessary initializations.
///
/// The string returned is the name of the plugin loaded, which can only be accessed once
/// the plugin has been loaded and calling the name method.
pub(crate) fn load_plugin(
    plugin_manager: &ArcSwap<GeyserPluginManager>,
    geyser_plugin_config_file: impl AsRef<Path>,
) -> JsonRpcResult<String> {
    let mut new_plugin_manager = (*plugin_manager.load_full()).clone();

    // First load plugin
    let (mut new_plugin, new_config_file) =
        load_plugin_from_config(geyser_plugin_config_file.as_ref()).map_err(|e| {
            jsonrpc_core::Error {
                code: ErrorCode::InvalidRequest,
                message: format!("Failed to load plugin: {e}"),
                data: None,
            }
        })?;

    // Then see if a plugin with this name already exists. If so, abort
    if new_plugin_manager
        .plugins()
        .iter()
        .any(|plugin| plugin.name().eq(new_plugin.name()))
    {
        return Err(jsonrpc_core::Error {
            code: ErrorCode::InvalidRequest,
            message: format!(
                "There already exists a plugin named {} loaded. Did not load requested plugin",
                new_plugin.name()
            ),
            data: None,
        });
    }

    setup_logger_for_plugin(&**new_plugin)?;

    // Call on_load and push plugin
    new_plugin
        .on_load(new_config_file, false)
        .map_err(|on_load_err| jsonrpc_core::Error {
            code: ErrorCode::InvalidRequest,
            message: format!(
                "on_load method of plugin {} failed: {on_load_err}",
                new_plugin.name()
            ),
            data: None,
        })?;
    let name = new_plugin.name().to_string();
    new_plugin_manager.push_plugin(new_plugin);
    plugin_manager.store(Arc::new(new_plugin_manager));

    Ok(name)
}

pub(crate) fn unload_plugin(
    plugin_manager: &ArcSwap<GeyserPluginManager>,
    name: &str,
) -> JsonRpcResult<()> {
    let mut new_plugin_manager: GeyserPluginManager = (*plugin_manager.load_full()).clone();

    // Check if any plugin names match this one
    let Some(idx) = new_plugin_manager
        .plugins()
        .iter()
        .position(|plugin| plugin.name().eq(name))
    else {
        // If we don't find one return an error
        return Err(jsonrpc_core::error::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("The plugin you requested to unload is not loaded"),
            data: None,
        });
    };

    // Unload and drop plugin and lib
    let plugin_ref = new_plugin_manager.remove_plugin(idx);
    plugin_manager.store(Arc::new(new_plugin_manager));
    GeyserPluginManager::unload_plugin_blocking(plugin_ref, idx);

    Ok(())
}

/// Checks for a plugin with a given `name`.
/// If it exists, first unload it.
/// Then, attempt to load a new plugin
/// Returns a new instance of GeyserPluginManager
pub(crate) fn reload_plugin(
    plugin_manager: &ArcSwap<GeyserPluginManager>,
    name: &str,
    config_file: &str,
) -> JsonRpcResult<()> {
    let mut new_plugin_manager: GeyserPluginManager = (*plugin_manager.load_full()).clone();
    // Check if any plugin names match this one
    let Some(idx) = new_plugin_manager
        .plugins()
        .iter()
        .position(|plugin| plugin.name().eq(name))
    else {
        // If we don't find one return an error
        return Err(jsonrpc_core::error::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("The plugin you requested to reload is not loaded"),
            data: None,
        });
    };

    // Unload and drop current plugin first in case plugin requires exclusive access to resource,
    // such as a particular port or database.
    let plugin_ref = new_plugin_manager.remove_plugin(idx);
    // store a cloned instance of the plugin manager without the plugin while we are reloading the plugin
    // this ensures that the plugin is not called/updated after we unload it
    plugin_manager.store(Arc::new(new_plugin_manager.clone()));
    GeyserPluginManager::unload_plugin_blocking(plugin_ref, idx);

    // Try to load the plugin, library
    // SAFETY: It is up to the validator to ensure this is a valid plugin library.
    let (mut new_plugin, new_parsed_config_file) = load_plugin_from_config(config_file.as_ref())
        .map_err(|err| jsonrpc_core::Error {
            code: ErrorCode::InvalidRequest,
            message: err.to_string(),
            data: None,
        })?;

    // Then see if a plugin with this name already exists. If so, abort
    if new_plugin_manager
        .plugins()
        .iter()
        .any(|plugin| plugin.name().eq(new_plugin.name()))
    {
        return Err(jsonrpc_core::Error {
            code: ErrorCode::InvalidRequest,
            message: format!(
                "There already exists a plugin named {} loaded, while reloading {name}. Did not \
                 load requested plugin",
                new_plugin.name()
            ),
            data: None,
        });
    }

    setup_logger_for_plugin(&**new_plugin)?;

    // Attempt to on_load with new plugin
    match new_plugin.on_load(new_parsed_config_file, true) {
        // On success, push plugin and library
        Ok(()) => {
            new_plugin_manager.push_plugin(new_plugin);
            plugin_manager.store(Arc::new(new_plugin_manager));
        }

        // On failure, return error
        Err(err) => {
            return Err(jsonrpc_core::error::Error {
                code: ErrorCode::InvalidRequest,
                message: format!(
                    "Failed to start new plugin (previous plugin was dropped!): {err}"
                ),
                data: None,
            });
        }
    }

    Ok(())
}

// Initialize logging for the plugin
fn setup_logger_for_plugin(new_plugin: &dyn GeyserPlugin) -> Result<(), jsonrpc_core::Error> {
    new_plugin
        .setup_logger(log::logger(), log::max_level())
        .map_err(|setup_logger_err| jsonrpc_core::Error {
            code: ErrorCode::InvalidRequest,
            message: format!(
                "setup_logger method of plugin {} failed: {setup_logger_err}",
                new_plugin.name()
            ),
            data: None,
        })
}

#[derive(Debug)]
pub enum GeyserPluginManagerRequest {
    ReloadPlugin {
        name: String,
        config_file: String,
        response_sender: OneShotSender<JsonRpcResult<()>>,
    },
    UnloadPlugin {
        name: String,
        response_sender: OneShotSender<JsonRpcResult<()>>,
    },
    LoadPlugin {
        config_file: String,
        response_sender: OneShotSender<JsonRpcResult<String>>,
    },
    ListPlugins {
        response_sender: OneShotSender<JsonRpcResult<Vec<String>>>,
    },
}

#[cfg(test)]
const TESTPLUGIN_CONFIG: &str = "TESTPLUGIN_CONFIG";
#[cfg(test)]
const TESTPLUGIN2_CONFIG: &str = "TESTPLUGIN2_CONFIG";

// This is mocked for tests to avoid having to do IO with a dynamically linked library
// across different architectures at test time
//
/// This returns mocked values for the geyser plugin, the dynamic library, and the parsed config file as a &str.
/// (The geyser plugin interface requires a &str for the on_load method).
#[cfg(test)]
pub(crate) fn load_plugin_from_config(
    geyser_plugin_config_file: &Path,
) -> Result<(LoadedGeyserPlugin, &str), GeyserPluginManagerError> {
    if geyser_plugin_config_file.ends_with(TESTPLUGIN_CONFIG) {
        Ok(tests::dummy_plugin_and_library(
            tests::TestPlugin::default(),
            TESTPLUGIN_CONFIG,
        ))
    } else if geyser_plugin_config_file.ends_with(TESTPLUGIN2_CONFIG) {
        Ok(tests::dummy_plugin_and_library(
            tests::TestPlugin2::default(),
            TESTPLUGIN2_CONFIG,
        ))
    } else {
        Err(GeyserPluginManagerError::CannotOpenConfigFile(
            geyser_plugin_config_file.to_str().unwrap().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            deshred_transaction_notifier::DeshredTransactionNotifierImpl,
            geyser_plugin_manager::{
                TESTPLUGIN_CONFIG, TESTPLUGIN2_CONFIG, list_plugins, load_plugin, reload_plugin,
                unload_plugin,
            },
            geyser_plugin_service::ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION,
        },
        agave_geyser_plugin_host::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, ReplicaDeshredTransactionInfo, ReplicaDeshredTransactionInfoVersions,
            ReplicaDeshredUpdateParentInfoVersions, Result as PluginResult,
        },
        arc_swap::ArcSwap,
        libloading::Library,
        solana_clock::Slot,
        solana_hash::Hash,
        solana_ledger::{
            blockstore_meta::UpdateParentInfo,
            deshred_transaction_notifier_interface::DeshredTransactionNotifier,
        },
        solana_message::{Instruction, Message, VersionedMessage, v0::LoadedAddresses},
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_transaction::versioned::VersionedTransaction,
        std::sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    pub(super) fn dummy_plugin_and_library<P: GeyserPlugin>(
        plugin: P,
        config_path: &'static str,
    ) -> (LoadedGeyserPlugin, &'static str) {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();
        (
            LoadedGeyserPlugin::new(Library::from(library), Box::new(plugin), None),
            config_path,
        )
    }

    const DUMMY_NAME: &str = "dummy";
    pub(super) const DUMMY_CONFIG: &str = "dummy_config";
    const ANOTHER_DUMMY_NAME: &str = "another_dummy";

    #[derive(Clone, Debug, Default)]
    pub(super) struct TestPlugin {
        loaded: Arc<AtomicBool>,
    }

    impl GeyserPlugin for TestPlugin {
        fn on_load(
            &mut self,
            _config_file: &str,
            _is_reload: bool,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            self.loaded.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn name(&self) -> &'static str {
            DUMMY_NAME
        }

        fn on_unload(&mut self) {
            self.loaded.store(false, Ordering::Relaxed)
        }
    }

    #[derive(Clone, Debug, Default)]
    pub(super) struct TestPlugin2 {
        loaded: Arc<AtomicBool>,
    }

    impl GeyserPlugin for TestPlugin2 {
        fn on_load(
            &mut self,
            _config_file: &str,
            _is_reload: bool,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            self.loaded.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn name(&self) -> &'static str {
            ANOTHER_DUMMY_NAME
        }

        fn on_unload(&mut self) {
            self.loaded.store(false, Ordering::Relaxed)
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedDeshredNotification {
        slot: Slot,
        completed_data_set_starting_shred_index: u32,
        completed_data_set_ending_shred_index_exclusive: u32,
        signature: Signature,
        is_vote: bool,
        transaction: VersionedTransaction,
        loaded_addresses: Option<LoadedAddresses>,
    }

    type DeshredUpdateParent = (Slot, u32, Slot, Hash);

    #[derive(Clone, Debug)]
    struct DeshredTestPlugin {
        name: &'static str,
        enabled: bool,
        alt_resolution_enabled: bool,
        notifications: Arc<Mutex<Vec<RecordedDeshredNotification>>>,
        update_parents: Arc<Mutex<Vec<DeshredUpdateParent>>>,
    }

    impl GeyserPlugin for DeshredTestPlugin {
        fn name(&self) -> &'static str {
            self.name
        }

        fn notify_deshred_transaction(
            &self,
            transaction: ReplicaDeshredTransactionInfoVersions,
            slot: Slot,
        ) -> PluginResult<()> {
            let ReplicaDeshredTransactionInfoVersions::V0_0_2(transaction) = transaction else {
                panic!("expected V0_0_2 deshred transaction info");
            };
            self.notifications
                .lock()
                .unwrap()
                .push(RecordedDeshredNotification {
                    slot,
                    completed_data_set_starting_shred_index: transaction
                        .completed_data_set_starting_shred_index,
                    completed_data_set_ending_shred_index_exclusive: transaction
                        .completed_data_set_ending_shred_index_exclusive,
                    signature: *transaction.signature,
                    is_vote: transaction.is_vote,
                    transaction: transaction.transaction.clone(),
                    loaded_addresses: transaction.loaded_addresses.cloned(),
                });
            Ok(())
        }

        fn deshred_transaction_notifications_enabled(&self) -> bool {
            self.enabled
        }

        fn notify_deshred_update_parent(
            &self,
            update_parent: ReplicaDeshredUpdateParentInfoVersions,
        ) -> PluginResult<()> {
            let ReplicaDeshredUpdateParentInfoVersions::V0_0_1(update_parent) = update_parent;
            self.update_parents.lock().unwrap().push((
                update_parent.slot,
                update_parent.update_parent_fec_set_index,
                update_parent.parent_slot,
                *update_parent.parent_block_id,
            ));
            Ok(())
        }

        fn deshred_transaction_alt_resolution_enabled(&self) -> bool {
            self.alt_resolution_enabled
        }
    }

    fn sample_transaction() -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::from([1; 64])],
            message: VersionedMessage::Legacy(Message::new(
                &[Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    Vec::new(),
                )],
                Some(&Pubkey::new_unique()),
            )),
        }
    }

    #[test]
    fn test_geyser_reload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // No plugins are loaded, this should fail
        let reload_result = reload_plugin(&plugin_manager, DUMMY_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Mock having loaded plugin (TestPlugin)
        let test_plugin_loaded = Arc::new(AtomicBool::new(false));
        let test_plugin = TestPlugin {
            loaded: test_plugin_loaded.clone(),
        };
        let (mut plugin, config) = dummy_plugin_and_library(test_plugin, DUMMY_CONFIG);
        plugin.on_load(config, false).unwrap();
        assert!(test_plugin_loaded.load(Ordering::Relaxed));
        let mut new_plugin_manager = (**plugin_manager.load()).clone();
        new_plugin_manager.push_plugin(plugin);
        assert_eq!(new_plugin_manager.plugins()[0].name(), DUMMY_NAME);
        new_plugin_manager.plugins()[0].name();
        plugin_manager.store(Arc::new(new_plugin_manager));

        // Try wrong name (same error)
        const WRONG_NAME: &str = "wrong_name";
        let reload_result = reload_plugin(&plugin_manager, WRONG_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Now try a (dummy) reload, replacing TestPlugin with TestPlugin2
        let reload_result = reload_plugin(&plugin_manager, DUMMY_NAME, TESTPLUGIN2_CONFIG);
        assert!(reload_result.is_ok());
        assert!(!test_plugin_loaded.load(Ordering::Relaxed));

        // The plugin is now replaced with ANOTHER_DUMMY_NAME
        let plugins = list_plugins(&plugin_manager.load()).unwrap();
        assert!(plugins.iter().any(|name| name.eq(ANOTHER_DUMMY_NAME)));
        // DUMMY_NAME should no longer be present.
        assert!(!plugins.iter().any(|name| name.eq(DUMMY_NAME)));
    }

    #[test]
    fn test_plugin_list() {
        // Initialize empty manager
        let plugin_manager = Arc::new(RwLock::new(GeyserPluginManager::default()));
        let mut plugin_manager_lock = plugin_manager.write().unwrap();

        // Load two plugins
        // First
        let (mut plugin, config) =
            dummy_plugin_and_library(TestPlugin::default(), TESTPLUGIN_CONFIG);
        plugin.on_load(config, false).unwrap();
        plugin_manager_lock.push_plugin(plugin);
        // Second
        let (mut plugin, config) =
            dummy_plugin_and_library(TestPlugin2::default(), TESTPLUGIN2_CONFIG);
        plugin.on_load(config, false).unwrap();
        plugin_manager_lock.push_plugin(plugin);

        // Check that both plugins are returned in the list
        let plugins = list_plugins(&plugin_manager_lock).unwrap();
        assert!(plugins.iter().any(|name| name.eq(DUMMY_NAME)));
        assert!(plugins.iter().any(|name| name.eq(ANOTHER_DUMMY_NAME)));
    }

    #[test]
    fn test_plugin_load_unload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // Load rpc call
        let load_result = load_plugin(&plugin_manager, TESTPLUGIN_CONFIG);
        assert!(load_result.is_ok());
        assert_eq!(plugin_manager.load().plugins().len(), 1);

        // Unload rpc call
        let unload_result = unload_plugin(&plugin_manager, DUMMY_NAME);
        assert!(unload_result.is_ok());
        assert_eq!(plugin_manager.load().plugins().len(), 0);
    }

    #[test]
    fn test_load_plugin_rejects_duplicate_name() {
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));
        load_plugin(&plugin_manager, TESTPLUGIN_CONFIG).unwrap();

        let err = load_plugin(&plugin_manager, TESTPLUGIN_CONFIG)
            .expect_err("a second plugin with the same name must be rejected");
        assert!(err.message.contains("already exists"), "{}", err.message);
        assert_eq!(plugin_manager.load().plugins().len(), 1);
    }

    #[test]
    fn test_load_plugin_reports_config_errors() {
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));
        let err = load_plugin(&plugin_manager, "not_a_known_config")
            .expect_err("an unloadable config must be reported");
        assert!(
            err.message.starts_with("Failed to load plugin"),
            "{}",
            err.message
        );
        assert!(plugin_manager.load().plugins().is_empty());
    }

    #[test]
    fn test_unload_and_reload_reject_unknown_plugin() {
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        let err = unload_plugin(&plugin_manager, "nope").expect_err("nothing to unload");
        assert!(err.message.contains("not loaded"), "{}", err.message);

        let err = reload_plugin(&plugin_manager, "nope", TESTPLUGIN_CONFIG)
            .expect_err("nothing to reload");
        assert!(err.message.contains("not loaded"), "{}", err.message);
    }

    /// Reload unloads the old plugin before loading the new config, so a config error
    /// leaves the manager without the plugin rather than with the stale one.
    #[test]
    fn test_reload_plugin_config_error_drops_previous_plugin() {
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));
        load_plugin(&plugin_manager, TESTPLUGIN_CONFIG).unwrap();

        let err = reload_plugin(&plugin_manager, DUMMY_NAME, "not_a_known_config")
            .expect_err("reload with an unloadable config must fail");
        assert!(!err.message.is_empty());
        assert!(plugin_manager.load().plugins().is_empty());
    }

    #[test]
    fn test_reload_plugin_rejects_duplicate_name() {
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));
        load_plugin(&plugin_manager, TESTPLUGIN_CONFIG).unwrap();
        load_plugin(&plugin_manager, TESTPLUGIN2_CONFIG).unwrap();

        // Reloading `dummy` from a config that yields `another_dummy` collides with the
        // plugin that is still loaded; `dummy` has already been unloaded by then.
        let err = reload_plugin(&plugin_manager, DUMMY_NAME, TESTPLUGIN2_CONFIG)
            .expect_err("reload must not produce two plugins with the same name");
        assert!(err.message.contains("already exists"), "{}", err.message);
        let names: Vec<_> = plugin_manager
            .load()
            .plugins()
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert_eq!(names, vec![ANOTHER_DUMMY_NAME.to_string()]);
    }

    #[test]
    fn test_deshred_transaction_notifications_enabled() {
        let empty_manager = GeyserPluginManager::default();
        assert!(!empty_manager.deshred_transaction_notifications_enabled());

        let disabled_manager = GeyserPluginManager::from_plugins(vec![Arc::new(
            dummy_plugin_and_library(
                DeshredTestPlugin {
                    name: DUMMY_NAME,
                    enabled: false,
                    alt_resolution_enabled: false,
                    notifications: Arc::new(Mutex::new(Vec::new())),
                    update_parents: Arc::new(Mutex::new(Vec::new())),
                },
                DUMMY_CONFIG,
            )
            .0,
        )]);
        assert!(!disabled_manager.deshred_transaction_notifications_enabled());

        let enabled_manager = GeyserPluginManager::from_plugins(vec![Arc::new(
            dummy_plugin_and_library(
                DeshredTestPlugin {
                    name: ANOTHER_DUMMY_NAME,
                    enabled: true,
                    alt_resolution_enabled: false,
                    notifications: Arc::new(Mutex::new(Vec::new())),
                    update_parents: Arc::new(Mutex::new(Vec::new())),
                },
                DUMMY_CONFIG,
            )
            .0,
        )]);
        assert!(enabled_manager.deshred_transaction_notifications_enabled());
    }

    #[test]
    fn test_deshred_transaction_notifier_forwards_only_enabled_plugins() {
        let enabled_notifications = Arc::new(Mutex::new(Vec::new()));
        let disabled_notifications = Arc::new(Mutex::new(Vec::new()));
        let enabled_update_parents = Arc::new(Mutex::new(Vec::new()));
        let disabled_update_parents = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::from_plugins(
            vec![
                Arc::new(
                    dummy_plugin_and_library(
                        DeshredTestPlugin {
                            name: DUMMY_NAME,
                            enabled: true,
                            alt_resolution_enabled: false,
                            notifications: enabled_notifications.clone(),
                            update_parents: enabled_update_parents.clone(),
                        },
                        DUMMY_CONFIG,
                    )
                    .0,
                ),
                Arc::new(
                    dummy_plugin_and_library(
                        DeshredTestPlugin {
                            name: ANOTHER_DUMMY_NAME,
                            enabled: false,
                            alt_resolution_enabled: false,
                            notifications: disabled_notifications.clone(),
                            update_parents: disabled_update_parents.clone(),
                        },
                        DUMMY_CONFIG,
                    )
                    .0,
                ),
            ],
        ))));
        let notifier = DeshredTransactionNotifierImpl::new(plugin_manager);
        let transaction = sample_transaction();
        let loaded_addresses = LoadedAddresses::default();

        notifier.notify_deshred_transaction(
            11,
            23,
            31,
            &transaction.signatures[0],
            true,
            &transaction,
            Some(&loaded_addresses),
        );
        let parent_block_id = Hash::new_unique();
        notifier.notify_deshred_update_parent(&UpdateParentInfo {
            slot: 11,
            update_parent_fec_set_index: 32,
            parent_slot: 9,
            parent_block_id,
        });

        let enabled_notifications = enabled_notifications.lock().unwrap().clone();
        assert_eq!(enabled_notifications.len(), 1);
        assert_eq!(enabled_notifications[0].slot, 11);
        assert_eq!(
            enabled_notifications[0].completed_data_set_starting_shred_index,
            23
        );
        assert_eq!(
            enabled_notifications[0].completed_data_set_ending_shred_index_exclusive,
            31
        );
        assert_eq!(
            enabled_notifications[0].signature,
            transaction.signatures[0]
        );
        assert!(enabled_notifications[0].is_vote);
        assert_eq!(enabled_notifications[0].transaction, transaction);
        assert_eq!(
            enabled_notifications[0].loaded_addresses,
            Some(loaded_addresses)
        );
        assert!(disabled_notifications.lock().unwrap().is_empty());
        assert_eq!(
            *enabled_update_parents.lock().unwrap(),
            vec![(11, 32, 9, parent_block_id)]
        );
        assert!(disabled_update_parents.lock().unwrap().is_empty());
    }

    #[test]
    #[should_panic(expected = "expected V0_0_2 deshred transaction info")]
    fn test_deshred_test_plugin_panics_on_legacy_deshred_info_version() {
        let plugin = DeshredTestPlugin {
            name: DUMMY_NAME,
            enabled: true,
            alt_resolution_enabled: false,
            notifications: Arc::new(Mutex::new(Vec::new())),
            update_parents: Arc::new(Mutex::new(Vec::new())),
        };
        let transaction = sample_transaction();
        let deshred_info = ReplicaDeshredTransactionInfo {
            signature: &transaction.signatures[0],
            is_vote: false,
            transaction: &transaction,
            loaded_addresses: None,
        };

        let _ = plugin.notify_deshred_transaction(
            ReplicaDeshredTransactionInfoVersions::V0_0_1(&deshred_info),
            11,
        );
    }

    #[test]
    fn test_deshred_transaction_alt_resolution_enabled() {
        let empty_manager = GeyserPluginManager::default();
        assert!(!empty_manager.deshred_transaction_alt_resolution_enabled());

        let disabled_manager = GeyserPluginManager::from_plugins(vec![Arc::new(
            dummy_plugin_and_library(
                DeshredTestPlugin {
                    name: DUMMY_NAME,
                    enabled: true,
                    alt_resolution_enabled: false,
                    notifications: Arc::new(Mutex::new(Vec::new())),
                    update_parents: Arc::new(Mutex::new(Vec::new())),
                },
                DUMMY_CONFIG,
            )
            .0,
        )]);
        assert!(!disabled_manager.deshred_transaction_alt_resolution_enabled());

        let enabled_manager = GeyserPluginManager::from_plugins(vec![Arc::new(
            dummy_plugin_and_library(
                DeshredTestPlugin {
                    name: ANOTHER_DUMMY_NAME,
                    enabled: true,
                    alt_resolution_enabled: true,
                    notifications: Arc::new(Mutex::new(Vec::new())),
                    update_parents: Arc::new(Mutex::new(Vec::new())),
                },
                DUMMY_CONFIG,
            )
            .0,
        )]);
        assert!(enabled_manager.deshred_transaction_alt_resolution_enabled());
    }

    #[test]
    fn test_geyser_plugin_manager_reload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // No plugins are loaded, this should fail
        let reload_result = reload_plugin(&plugin_manager, DUMMY_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Mock having loaded plugin (TestPlugin)
        let test_plugin_loaded = Arc::new(AtomicBool::new(false));
        let test_plugin = TestPlugin {
            loaded: test_plugin_loaded.clone(),
        };
        let (mut plugin, config) = dummy_plugin_and_library(test_plugin, DUMMY_CONFIG);
        plugin.on_load(config, false).unwrap();
        assert!(test_plugin_loaded.load(Ordering::Relaxed));
        let mut new_plugin_manager = (**plugin_manager.load()).clone();
        new_plugin_manager.push_plugin(plugin);
        assert_eq!(new_plugin_manager.plugins()[0].name(), DUMMY_NAME);
        new_plugin_manager.plugins()[0].name();
        plugin_manager.store(Arc::new(new_plugin_manager));

        // check that plugin gets unloaded when we unload the plugin manager
        let empty_plugin_manager = GeyserPluginManager::default();
        let mut geyser_plugin_manager_ref = plugin_manager.swap(Arc::new(empty_plugin_manager));
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
            std::thread::sleep(ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION);
        }
        assert!(!test_plugin_loaded.load(Ordering::Relaxed));
    }
}
