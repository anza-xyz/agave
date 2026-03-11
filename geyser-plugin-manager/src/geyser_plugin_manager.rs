use {
    crate::geyser_plugin_service::ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION,
    agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
    arc_swap::ArcSwap,
    jsonrpc_core::{ErrorCode, Result as JsonRpcResult},
    libloading::Library,
    log::*,
    std::{
        ops::{Deref, DerefMut},
        path::Path,
        sync::Arc,
        thread,
    },
    tokio::sync::oneshot::Sender as OneShotSender,
};

#[derive(Debug)]
pub struct LoadedGeyserPlugin {
    name: String,
    plugin: Box<dyn GeyserPlugin>,
    // NOTE: While we do not access the library, the plugin we have loaded most
    // certainly does. To ensure we don't SIGSEGV we must declare the library
    // after the plugin so the plugin is dropped first.
    //
    // Furthermore, a well behaved Geyser plugin must ensure it ceases to run
    // any code before returning from Drop. This means if the Geyser plugins
    // spawn threads that access the Library, those threads must be `join`ed
    // before the Geyser plugin returns from on_unload / Drop.
    #[allow(dead_code)]
    library: Library,
}

impl LoadedGeyserPlugin {
    pub fn new(library: Library, plugin: Box<dyn GeyserPlugin>, name: Option<String>) -> Self {
        Self {
            name: name.unwrap_or_else(|| plugin.name().to_owned()),
            plugin,
            library,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Deref for LoadedGeyserPlugin {
    type Target = Box<dyn GeyserPlugin>;

    fn deref(&self) -> &Self::Target {
        &self.plugin
    }
}

impl DerefMut for LoadedGeyserPlugin {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.plugin
    }
}

#[derive(Default, Debug, Clone)]
pub struct GeyserPluginManager {
    pub plugins: Vec<Arc<LoadedGeyserPlugin>>,
}

impl GeyserPluginManager {
    /// Unload all plugins and loaded plugin libraries, making sure to fire
    /// their `on_plugin_unload()` methods so they can do any necessary cleanup.
    pub fn unload(&mut self) {
        for (idx, plugin) in self.plugins.drain(..).enumerate() {
            info!("Unloading plugin for {:?}", plugin.name());
            Self::unload_plugin_blocking(plugin, idx);
        }
    }

    /// Check if there is any plugin interested in account data
    pub fn account_data_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.account_data_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Check if there is any plugin interested in account data from snapshot
    pub fn account_data_snapshot_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.account_data_snapshot_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Check if there is any plugin interested in transaction data
    pub fn transaction_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.transaction_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Check if there is any plugin interested in entry data
    pub fn entry_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.entry_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Admin RPC request handler
    pub(crate) fn list_plugins(&self) -> JsonRpcResult<Vec<String>> {
        Ok(self.plugins.iter().map(|p| p.name().to_owned()).collect())
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
            .plugins
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

        setup_logger_for_plugin(&*new_plugin.plugin)?;

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
        new_plugin_manager.plugins.push(Arc::new(new_plugin));
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
            .plugins
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
        let plugin_ref = new_plugin_manager.plugins.remove(idx);
        plugin_manager.store(Arc::new(new_plugin_manager));
        Self::unload_plugin_blocking(plugin_ref, idx);

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
            .plugins
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
        let plugin_ref = new_plugin_manager.plugins.remove(idx);
        // store a cloned instance of the plugin manager without the plugin while we are reloading the plugin
        // this ensures that the plugin is not called/updated after we unload it
        plugin_manager.store(Arc::new(new_plugin_manager.clone()));
        Self::unload_plugin_blocking(plugin_ref, idx);

        // Try to load the plugin, library
        // SAFETY: It is up to the validator to ensure this is a valid plugin library.
        let (mut new_plugin, new_parsed_config_file) =
            load_plugin_from_config(config_file.as_ref()).map_err(|err| jsonrpc_core::Error {
                code: ErrorCode::InvalidRequest,
                message: err.to_string(),
                data: None,
            })?;

        // Then see if a plugin with this name already exists. If so, abort
        if new_plugin_manager
            .plugins
            .iter()
            .any(|plugin| plugin.name().eq(new_plugin.name()))
        {
            return Err(jsonrpc_core::Error {
                code: ErrorCode::InvalidRequest,
                message: format!(
                    "There already exists a plugin named {} loaded, while reloading {name}. Did \
                     not load requested plugin",
                    new_plugin.name()
                ),
                data: None,
            });
        }

        setup_logger_for_plugin(&*new_plugin.plugin)?;

        // Attempt to on_load with new plugin
        match new_plugin.on_load(new_parsed_config_file, true) {
            // On success, push plugin and library
            Ok(()) => {
                new_plugin_manager.plugins.push(Arc::new(new_plugin));
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

    /// Blocks the thread and unloads a given plugin.
    /// This synchronously and explicitly waits to hold the last Arc reference
    /// to the plugin before allowing it to be dropped and unloaded. This ensures
    /// that once this function returns, the plugin is fully unloaded.
    pub(crate) fn unload_plugin_blocking(mut plugin_ref: Arc<LoadedGeyserPlugin>, idx: usize) {
        loop {
            match Arc::try_unwrap(plugin_ref) {
                Ok(mut current_plugin) => {
                    let name = current_plugin.name().to_string();
                    current_plugin.plugin.on_unload();
                    info!("Unloaded plugin {name} at idx {idx}");
                    return;
                }
                Err(plugin_reference) => plugin_ref = plugin_reference,
            }
            thread::sleep(ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION);
        }
    }
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

#[derive(thiserror::Error, Debug)]
pub enum GeyserPluginManagerError {
    #[error("Cannot open the plugin config file")]
    CannotOpenConfigFile(String),

    #[error("Cannot read the plugin config file")]
    CannotReadConfigFile(String),

    #[error("The config file is not in a valid Json format")]
    InvalidConfigFileFormat(String),

    #[error("Plugin library path is not specified in the config file")]
    LibPathNotSet,

    #[error("Invalid plugin path")]
    InvalidPluginPath,

    #[error("Cannot load plugin shared library (error: {0})")]
    PluginLoadError(String),

    #[error("The geyser plugin {0} is already loaded shared library")]
    PluginAlreadyLoaded(String),

    #[error("The GeyserPlugin on_load method failed (error: {0})")]
    PluginStartError(String),
}

/// # Safety
///
/// This function loads the dynamically linked library specified in the path. The library
/// must do necessary initializations.
///
/// This returns the geyser plugin, the dynamic library, and the parsed config file as a &str.
/// (The geyser plugin interface requires a &str for the on_load method).
#[cfg(not(test))]
pub(crate) fn load_plugin_from_config(
    geyser_plugin_config_file: &Path,
) -> Result<(LoadedGeyserPlugin, &str), GeyserPluginManagerError> {
    use std::{fs::File, io::Read, path::PathBuf};
    type PluginConstructor = unsafe fn() -> *mut dyn GeyserPlugin;
    use libloading::Symbol;

    let mut file = match File::open(geyser_plugin_config_file) {
        Ok(file) => file,
        Err(err) => {
            return Err(GeyserPluginManagerError::CannotOpenConfigFile(format!(
                "Failed to open the plugin config file {geyser_plugin_config_file:?}, error: \
                 {err:?}"
            )));
        }
    };

    let mut contents = String::new();
    if let Err(err) = file.read_to_string(&mut contents) {
        return Err(GeyserPluginManagerError::CannotReadConfigFile(format!(
            "Failed to read the plugin config file {geyser_plugin_config_file:?}, error: {err:?}"
        )));
    }

    let result: serde_json::Value = match json5::from_str(&contents) {
        Ok(value) => value,
        Err(err) => {
            return Err(GeyserPluginManagerError::InvalidConfigFileFormat(format!(
                "The config file {geyser_plugin_config_file:?} is not in a valid Json5 format, \
                 error: {err:?}"
            )));
        }
    };

    let libpath = result["libpath"]
        .as_str()
        .ok_or(GeyserPluginManagerError::LibPathNotSet)?;
    let mut libpath = PathBuf::from(libpath);
    if libpath.is_relative() {
        let config_dir = geyser_plugin_config_file.parent().ok_or_else(|| {
            GeyserPluginManagerError::CannotOpenConfigFile(format!(
                "Failed to resolve parent of {geyser_plugin_config_file:?}",
            ))
        })?;
        libpath = config_dir.join(libpath);
    }

    let plugin_name = result["name"].as_str().map(|s| s.to_owned());

    let config_file = geyser_plugin_config_file
        .as_os_str()
        .to_str()
        .ok_or(GeyserPluginManagerError::InvalidPluginPath)?;

    let (plugin, lib) = unsafe {
        let lib = Library::new(libpath)
            .map_err(|e| GeyserPluginManagerError::PluginLoadError(e.to_string()))?;
        let constructor: Symbol<PluginConstructor> = lib
            .get(b"_create_plugin")
            .map_err(|e| GeyserPluginManagerError::PluginLoadError(e.to_string()))?;
        let plugin_raw = constructor();
        (Box::from_raw(plugin_raw), lib)
    };
    Ok((
        LoadedGeyserPlugin::new(lib, plugin, plugin_name),
        config_file,
    ))
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
            geyser_plugin_manager::{
                GeyserPluginManager, LoadedGeyserPlugin, TESTPLUGIN_CONFIG, TESTPLUGIN2_CONFIG,
            },
            geyser_plugin_service::ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION,
        },
        agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
        arc_swap::ArcSwap,
        libloading::Library,
        std::sync::{
            Arc, RwLock,
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

    #[test]
    fn test_geyser_reload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // No plugins are loaded, this should fail
        let reload_result =
            GeyserPluginManager::reload_plugin(&plugin_manager, DUMMY_NAME, DUMMY_CONFIG);
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
        new_plugin_manager.plugins.push(Arc::new(plugin));
        assert_eq!(new_plugin_manager.plugins[0].name(), DUMMY_NAME);
        new_plugin_manager.plugins[0].name();
        plugin_manager.store(Arc::new(new_plugin_manager));

        // Try wrong name (same error)
        const WRONG_NAME: &str = "wrong_name";
        let reload_result =
            GeyserPluginManager::reload_plugin(&plugin_manager, WRONG_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Now try a (dummy) reload, replacing TestPlugin with TestPlugin2
        let reload_result =
            GeyserPluginManager::reload_plugin(&plugin_manager, DUMMY_NAME, TESTPLUGIN2_CONFIG);
        assert!(reload_result.is_ok());
        assert!(!test_plugin_loaded.load(Ordering::Relaxed));

        // The plugin is now replaced with ANOTHER_DUMMY_NAME
        let plugins = plugin_manager.load().list_plugins().unwrap();
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
        plugin_manager_lock.plugins.push(Arc::new(plugin));
        // Second
        let (mut plugin, config) =
            dummy_plugin_and_library(TestPlugin2::default(), TESTPLUGIN2_CONFIG);
        plugin.on_load(config, false).unwrap();
        plugin_manager_lock.plugins.push(Arc::new(plugin));

        // Check that both plugins are returned in the list
        let plugins = plugin_manager_lock.list_plugins().unwrap();
        assert!(plugins.iter().any(|name| name.eq(DUMMY_NAME)));
        assert!(plugins.iter().any(|name| name.eq(ANOTHER_DUMMY_NAME)));
    }

    #[test]
    fn test_plugin_load_unload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // Load rpc call
        let load_result = GeyserPluginManager::load_plugin(&plugin_manager, TESTPLUGIN_CONFIG);
        assert!(load_result.is_ok());
        assert_eq!(plugin_manager.load().plugins.len(), 1);

        // Unload rpc call
        let unload_result = GeyserPluginManager::unload_plugin(&plugin_manager, DUMMY_NAME);
        assert!(unload_result.is_ok());
        assert_eq!(plugin_manager.load().plugins.len(), 0);
    }

    #[test]
    fn test_geyser_plugin_manager_reload() {
        // Initialize empty manager
        let plugin_manager = Arc::new(ArcSwap::new(Arc::new(GeyserPluginManager::default())));

        // No plugins are loaded, this should fail
        let reload_result =
            GeyserPluginManager::reload_plugin(&plugin_manager, DUMMY_NAME, DUMMY_CONFIG);
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
        new_plugin_manager.plugins.push(Arc::new(plugin));
        assert_eq!(new_plugin_manager.plugins[0].name(), DUMMY_NAME);
        new_plugin_manager.plugins[0].name();
        plugin_manager.store(Arc::new(new_plugin_manager));

        // check that plugin gets unloaded when we unload the plugin manager
        let empty_plugin_manager = GeyserPluginManager {
            plugins: Vec::new(),
        };
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
