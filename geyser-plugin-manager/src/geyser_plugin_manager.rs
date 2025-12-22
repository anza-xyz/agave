use {
    agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
    arc_swap::ArcSwap,
    jsonrpc_core::{ErrorCode, Result as JsonRpcResult},
    libloading::Library,
    log::*,
    std::{
        mem::MaybeUninit,
        ops::{Deref, DerefMut},
        path::Path,
        sync::Arc,
        time::Duration,
    },
    tokio::sync::oneshot::Sender as OneShotSender,
};

pub const MAX_LOADED_GEYSER_PLUGINS: usize = 16;

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

#[derive(Debug)]
struct InnerPluginTables {
    // This field is unused but must be kept to ensure the LoadedGeyserPlugin
    // instances are kept alive as long as InnerPluginTables is alive.
    // So is the deref_table below.
    plugins: [MaybeUninit<Arc<LoadedGeyserPlugin>>; MAX_LOADED_GEYSER_PLUGINS],
    deref_table: [MaybeUninit<*const dyn GeyserPlugin>; MAX_LOADED_GEYSER_PLUGINS],
    len: usize,
}

impl Drop for InnerPluginTables {
    fn drop(&mut self) {
        for i in 0..self.len {
            unsafe {
                self.plugins[i].assume_init_drop();
                self.deref_table[i] = MaybeUninit::zeroed();
            }
        }
        self.len = 0;
    }
}

impl Clone for InnerPluginTables {
    fn clone(&self) -> Self {
        let mut new_plugins = std::array::from_fn(|_| MaybeUninit::uninit());
        for i in 0..self.len {
            let plugin = unsafe { self.plugins[i].assume_init_ref().clone() };
            new_plugins[i] = MaybeUninit::new(plugin);
        }
        Self {
            plugins: new_plugins,
            deref_table: self.deref_table.clone(),
            len: self.len,
        }
    }
}

// unsafe impl Sync for HotGeyserPluginList {}
// Its safe to implement since GeyserPlugin is Send + Sync by definition,
// morever the lifetime is correct since HotGeyserPluginList owns the plugins array as long as it lives.
unsafe impl Send for InnerPluginTables {}
unsafe impl Sync for InnerPluginTables {}

impl InnerPluginTables {
    pub fn empty() -> Self {
        Self {
            plugins: std::array::from_fn(|_| MaybeUninit::uninit()),
            deref_table: std::array::from_fn(|_| MaybeUninit::uninit()),
            len: 0,
        }
    }
}

#[derive(Debug)]
pub struct HotGeyserPluginList {
    plugins: Option<InnerPluginTables>,
    // This table already dereferences the Arc, so we can avoid that cost when iterating over plugins
    drop_callback_tx: crossbeam_channel::Sender<InnerPluginTables>,
}

impl Drop for HotGeyserPluginList {
    fn drop(&mut self) {
        if let Some(plugins) = self.plugins.take() {
            if let Err(e) = self.drop_callback_tx.send(plugins) {
                error!(
                    "Failed to send drop callback for HotGeyserPluginList: {}",
                    e
                );
            }
        }
    }
}

pub struct HotGeyserPluginListIter<'a> {
    list: &'a [MaybeUninit<*const dyn GeyserPlugin>],
    index: usize,
}

impl<'a> Iterator for HotGeyserPluginListIter<'a> {
    type Item = &'a dyn GeyserPlugin;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.list.len() {
            return None;
        }
        let plugin = unsafe { self.list[self.index].assume_init().as_ref() }.expect("null ptr");
        self.index += 1;
        Some(plugin)
    }
}

impl HotGeyserPluginList {
    pub fn is_empty(&self) -> bool {
        self.plugins.as_ref().map(|p| p.len == 0).unwrap_or(true)
    }

    pub fn iter(&self) -> HotGeyserPluginListIter<'_> {
        HotGeyserPluginListIter {
            list: &self
                .plugins
                .as_ref()
                .map(|p| &p.deref_table[..p.len])
                .unwrap_or(&[]),
            index: 0,
        }
    }
}

///
/// Geyser Plugin Manager
///
/// # Note
///
/// This struct is designed to be used behind a Mutex to allow safe concurrent access.
///
/// hot_plugin_list provides a lock-free read access to the list of loaded plugins for
/// high-performance paths such as account updates, transactions, and entries.
///
/// # Total pointers dereferenced per access
///
/// - 1 pointer dereference to get to the ArcSwap
/// - 1 pointer dereference to get to the HotGeyserPluginList
/// - 1 pointer dereference to get to the InnerPluginTables::deref_table (holds fat pointers to dyn GeyserPlugin)
/// - 1 pointer dereference to call methods on the dyn GeyserPlugin trait object
///
#[derive(Debug)]
pub struct GeyserPluginManager {
    plugins: Vec<Arc<LoadedGeyserPlugin>>,
    hot_plugin_list: Arc<ArcSwap<HotGeyserPluginList>>,
    drop_callback_tx: crossbeam_channel::Sender<InnerPluginTables>,
    drop_callback_rx: crossbeam_channel::Receiver<InnerPluginTables>,
}

impl Default for GeyserPluginManager {
    fn default() -> Self {
        let (drop_callback_tx, drop_callback_rx) = crossbeam_channel::bounded(1);
        GeyserPluginManager {
            plugins: Vec::new(),
            hot_plugin_list: Arc::new(ArcSwap::from_pointee(HotGeyserPluginList {
                plugins: Some(InnerPluginTables::empty()),
                drop_callback_tx: drop_callback_tx.clone(),
            })),
            drop_callback_tx,
            drop_callback_rx,
        }
    }
}

impl GeyserPluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get_hot_plugin_list(&self) -> Arc<ArcSwap<HotGeyserPluginList>> {
        Arc::clone(&self.hot_plugin_list)
    }

    /// Unload all plugins and loaded plugin libraries, making sure to fire
    /// their `on_plugin_unload()` methods so they can do any necessary cleanup.
    pub fn unload(&mut self) {
        let plugins = std::mem::take(&mut self.plugins);
        {
            let old = self.swap_plugin_list();
            drop(old);
        }

        for arc_plugin in plugins {
            let mut plugin = Arc::try_unwrap(arc_plugin).expect("Arc::try_unwrap");
            info!("Unloading plugin for {:?}", plugin.name());
            plugin.on_unload();
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
        &mut self,
        geyser_plugin_config_file: impl AsRef<Path>,
    ) -> JsonRpcResult<String> {
        if self.plugins.len() >= MAX_LOADED_GEYSER_PLUGINS {
            return Err(jsonrpc_core::Error {
                code: ErrorCode::InvalidRequest,
                message: format!("Cannot load more than {MAX_LOADED_GEYSER_PLUGINS} plugins",),
                data: None,
            });
        }

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
        if self
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
        self.plugins.push(Arc::new(new_plugin));

        // refresh shared plugin list
        let old = self.swap_plugin_list();
        drop(old);
        Ok(name)
    }

    pub(crate) fn unload_plugin(&mut self, name: &str) -> JsonRpcResult<()> {
        // Check if any plugin names match this one
        let Some(idx) = self
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
        self._drop_plugin(idx);

        Ok(())
    }

    /// Checks for a plugin with a given `name`.
    /// If it exists, first unload it.
    /// Then, attempt to load a new plugin
    pub(crate) fn reload_plugin(&mut self, name: &str, config_file: &str) -> JsonRpcResult<()> {
        // Check if any plugin names match this one
        let Some(idx) = self
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
        self._drop_plugin(idx);

        // Try to load plugin, library
        // SAFETY: It is up to the validator to ensure this is a valid plugin library.
        let (mut new_plugin, new_parsed_config_file) =
            load_plugin_from_config(config_file.as_ref()).map_err(|err| jsonrpc_core::Error {
                code: ErrorCode::InvalidRequest,
                message: err.to_string(),
                data: None,
            })?;

        // Then see if a plugin with this name already exists. If so, abort
        if self
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
                self.plugins.push(Arc::new(new_plugin));
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

        // refresh shared plugin list
        self.swap_plugin_list();
        Ok(())
    }

    fn build_plugin_list(&self) -> InnerPluginTables {
        let new_hot_plugin_inner_list =
            std::array::from_fn(|i| match self.plugins.get(i).cloned() {
                Some(plugin) => MaybeUninit::new(plugin),
                None => MaybeUninit::zeroed(),
            });
        let deref_table = std::array::from_fn(|i| {
            if let Some(plugin) = self.plugins.get(i) {
                let geyser_plugin = plugin.plugin.deref();
                MaybeUninit::new(geyser_plugin as *const dyn GeyserPlugin)
            } else {
                MaybeUninit::zeroed()
            }
        });
        InnerPluginTables {
            plugins: new_hot_plugin_inner_list,
            deref_table,
            len: self.plugins.len(),
        }
    }

    fn swap_plugin_list(&mut self) -> InnerPluginTables {
        let inner_tables = self.build_plugin_list();
        let new_hot_plugin_list = Arc::new(HotGeyserPluginList {
            plugins: Some(inner_tables),
            drop_callback_tx: self.drop_callback_tx.clone(),
        });
        log::info!(
            "Swapped hot plugin list with {} plugins",
            self.plugins.len()
        );
        self.hot_plugin_list.store(new_hot_plugin_list);
        self.drop_callback_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("Timeout waiting for old plugin list to be dropped")
    }

    fn _drop_plugin(&mut self, idx: usize) {
        let current_plugin = self.plugins.remove(idx);

        // Swap and drop the shared plugin list, this should decrements the ref count
        {
            let old = self.swap_plugin_list();
            info!(
                "Dropping old inner plugin tables with {} plugin refs",
                old.len
            );
            drop(old);
        }
        let mut current_plugin = Arc::try_unwrap(current_plugin).unwrap_or_else(|e| {
            panic!(
                "Failed to unwrap Arc for plugin {} at idx {}",
                e.name(),
                idx
            );
        });
        let name = current_plugin.name().to_string();
        current_plugin.plugin.on_unload();
        info!("Unloaded plugin {name} at idx {idx}");
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
            tests::TestPlugin,
            TESTPLUGIN_CONFIG,
        ))
    } else if geyser_plugin_config_file.ends_with(TESTPLUGIN2_CONFIG) {
        Ok(tests::dummy_plugin_and_library(
            tests::TestPlugin2,
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
        crate::geyser_plugin_manager::{
            GeyserPluginManager, LoadedGeyserPlugin, TESTPLUGIN2_CONFIG, TESTPLUGIN_CONFIG,
        },
        agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
        libloading::Library,
        std::sync::Arc,
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

    #[derive(Clone, Copy, Debug)]
    pub(super) struct TestPlugin;

    impl GeyserPlugin for TestPlugin {
        fn name(&self) -> &'static str {
            DUMMY_NAME
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) struct TestPlugin2;

    impl GeyserPlugin for TestPlugin2 {
        fn name(&self) -> &'static str {
            ANOTHER_DUMMY_NAME
        }
    }

    #[test]
    fn test_geyser_reload() {
        // Initialize empty manager
        let mut plugin_manager = GeyserPluginManager::default();

        // No plugins are loaded, this should fail
        let reload_result = plugin_manager.reload_plugin(DUMMY_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Mock having loaded plugin (TestPlugin)
        let (mut plugin, config) = dummy_plugin_and_library(TestPlugin, DUMMY_CONFIG);
        plugin.on_load(config, false).unwrap();
        plugin_manager.plugins.push(Arc::new(plugin));
        assert_eq!(plugin_manager.plugins[0].name(), DUMMY_NAME);
        plugin_manager.plugins[0].name();

        // Try wrong name (same error)
        const WRONG_NAME: &str = "wrong_name";
        let reload_result = plugin_manager.reload_plugin(WRONG_NAME, DUMMY_CONFIG);
        assert_eq!(
            reload_result.unwrap_err().message,
            "The plugin you requested to reload is not loaded"
        );

        // Now try a (dummy) reload, replacing TestPlugin with TestPlugin2
        let reload_result = plugin_manager.reload_plugin(DUMMY_NAME, TESTPLUGIN2_CONFIG);
        assert!(reload_result.is_ok());

        // The plugin is now replaced with ANOTHER_DUMMY_NAME
        let plugins = plugin_manager.list_plugins().unwrap();
        assert!(plugins.iter().any(|name| name.eq(ANOTHER_DUMMY_NAME)));
        // DUMMY_NAME should no longer be present.
        assert!(!plugins.iter().any(|name| name.eq(DUMMY_NAME)));
    }

    #[test]
    fn test_plugin_list() {
        // Initialize empty manager
        let mut plugin_manager = GeyserPluginManager::default();

        // Load two plugins
        // First
        plugin_manager.load_plugin(TESTPLUGIN_CONFIG).unwrap();
        // Second
        plugin_manager.load_plugin(TESTPLUGIN2_CONFIG).unwrap();

        // Check that both plugins are returned in the list
        let plugins = plugin_manager.list_plugins().unwrap();
        assert!(plugins.iter().any(|name| name.eq(DUMMY_NAME)));
        assert!(plugins.iter().any(|name| name.eq(ANOTHER_DUMMY_NAME)));
    }

    #[test]
    fn test_plugin_load_unload() {
        // Initialize empty manager
        let mut plugin_manager = GeyserPluginManager::default();

        // Load rpc call
        let load_result = plugin_manager.load_plugin(TESTPLUGIN_CONFIG);
        assert!(load_result.is_ok());
        assert_eq!(plugin_manager.plugins.len(), 1);
        assert_eq!(
            plugin_manager
                .hot_plugin_list
                .load()
                .plugins
                .as_ref()
                .unwrap()
                .len,
            1
        );

        // Unload rpc call
        let unload_result = plugin_manager.unload_plugin(DUMMY_NAME);
        assert!(unload_result.is_ok());
        assert_eq!(plugin_manager.plugins.len(), 0);
        assert_eq!(
            plugin_manager
                .hot_plugin_list
                .load()
                .plugins
                .as_ref()
                .unwrap()
                .len,
            0
        );
    }

    #[test]
    pub fn test_plugin_unload_all() {
        // Initialize empty manager
        let mut plugin_manager = GeyserPluginManager::default();

        // Load two plugins
        // First
        plugin_manager.load_plugin(TESTPLUGIN_CONFIG).unwrap();
        assert_eq!(plugin_manager.plugins.len(), 1);
        assert_eq!(
            plugin_manager
                .hot_plugin_list
                .load()
                .plugins
                .as_ref()
                .unwrap()
                .len,
            1
        );
        // Second
        plugin_manager.load_plugin(TESTPLUGIN2_CONFIG).unwrap();
        assert_eq!(plugin_manager.plugins.len(), 2);
        assert_eq!(
            plugin_manager
                .hot_plugin_list
                .load()
                .plugins
                .as_ref()
                .unwrap()
                .len,
            2
        );

        // Unload all
        plugin_manager.unload();

        // Check no plugins remain
        assert_eq!(plugin_manager.plugins.len(), 0);
        assert_eq!(
            plugin_manager
                .hot_plugin_list
                .load()
                .plugins
                .as_ref()
                .unwrap()
                .len,
            0
        );
    }
}
