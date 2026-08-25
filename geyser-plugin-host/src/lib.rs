//! Loads Geyser plugins and holds them for dispatch.
//!
//! This crate is the plugin *host*: it opens plugin shared libraries, owns the
//! loaded set, and answers which notifications any loaded plugin has asked for.
//! It depends only on the plugin ABI (`agave-geyser-plugin-interface`), never on
//! validator internals, so it can be built and used independently of the crates
//! that produce the notifications.
//!
//! Converting validator-internal types into the `Replica*` types this dispatches
//! is the job of the adapters in `solana-geyser-plugin-manager`, which stay on
//! the validator side.

use {
    agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin,
    libloading::Library,
    log::*,
    std::{
        ops::{Deref, DerefMut},
        path::Path,
        sync::Arc,
        thread,
        time::Duration,
    },
};

// How long to sleep between Arc::try_unwrap attempts
const ARC_TRY_UNWRAP_ATTEMPT_SLEEP_DURATION: Duration = Duration::from_millis(5);

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
    plugins: Vec<Arc<LoadedGeyserPlugin>>,
}

impl GeyserPluginManager {
    /// Builds a manager over an already-loaded set of plugins.
    pub fn from_plugins(plugins: Vec<Arc<LoadedGeyserPlugin>>) -> Self {
        Self { plugins }
    }

    /// The currently loaded plugins.
    pub fn plugins(&self) -> &[Arc<LoadedGeyserPlugin>] {
        &self.plugins
    }

    /// Adds an already-loaded plugin to the set.
    pub fn push_plugin(&mut self, plugin: LoadedGeyserPlugin) {
        self.plugins.push(Arc::new(plugin));
    }

    /// Removes the plugin at `idx`, panicking if it is out of bounds.
    pub fn remove_plugin(&mut self, idx: usize) -> Arc<LoadedGeyserPlugin> {
        self.plugins.remove(idx)
    }

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

    /// Check if there is any plugin interested in Alpenglow block footer data
    pub fn block_footer_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.block_footer_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Check if there is any plugin interested in deshred transaction data
    pub fn deshred_transaction_notifications_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.deshred_transaction_notifications_enabled() {
                return true;
            }
        }
        false
    }

    /// Check if there is any plugin interested in ALT resolution for deshred transactions
    pub fn deshred_transaction_alt_resolution_enabled(&self) -> bool {
        for plugin in &self.plugins {
            if plugin.deshred_transaction_alt_resolution_enabled() {
                return true;
            }
        }
        false
    }

    /// Blocks the thread and unloads a given plugin.
    /// This synchronously and explicitly waits to hold the last Arc reference
    /// to the plugin before allowing it to be dropped and unloaded. This ensures
    /// that once this function returns, the plugin is fully unloaded.
    pub fn unload_plugin_blocking(mut plugin_ref: Arc<LoadedGeyserPlugin>, idx: usize) {
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
pub fn load_plugin_from_config(
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
mod tests {
    use super::*;

    /// One flag per notification kind the manager fans in.
    #[derive(Debug, Default, Clone, Copy)]
    struct Kinds {
        accounts: bool,
        snapshot: bool,
        transactions: bool,
        entries: bool,
        block_footer: bool,
        deshred: bool,
        deshred_alt: bool,
    }

    #[derive(Debug)]
    struct TestPlugin {
        name: &'static str,
        kinds: Kinds,
    }

    impl GeyserPlugin for TestPlugin {
        fn name(&self) -> &'static str {
            self.name
        }
        fn account_data_notifications_enabled(&self) -> bool {
            self.kinds.accounts
        }
        fn account_data_snapshot_notifications_enabled(&self) -> bool {
            self.kinds.snapshot
        }
        fn transaction_notifications_enabled(&self) -> bool {
            self.kinds.transactions
        }
        fn entry_notifications_enabled(&self) -> bool {
            self.kinds.entries
        }
        fn block_footer_notifications_enabled(&self) -> bool {
            self.kinds.block_footer
        }
        fn deshred_transaction_notifications_enabled(&self) -> bool {
            self.kinds.deshred
        }
        fn deshred_transaction_alt_resolution_enabled(&self) -> bool {
            self.kinds.deshred_alt
        }
    }

    fn loaded(plugin: TestPlugin) -> LoadedGeyserPlugin {
        // `Library::this()` gives a handle to the current executable, so a test plugin can be
        // wrapped without loading anything from disk.
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();
        LoadedGeyserPlugin::new(Library::from(library), Box::new(plugin), None)
    }

    fn test_plugin(name: &'static str, accounts: bool, entries: bool) -> LoadedGeyserPlugin {
        loaded(TestPlugin {
            name,
            kinds: Kinds {
                accounts,
                entries,
                ..Kinds::default()
            },
        })
    }

    /// Reads all seven fan-ins of a manager in a fixed order.
    fn enabled_kinds(manager: &GeyserPluginManager) -> [bool; 7] {
        [
            manager.account_data_notifications_enabled(),
            manager.account_data_snapshot_notifications_enabled(),
            manager.transaction_notifications_enabled(),
            manager.entry_notifications_enabled(),
            manager.block_footer_notifications_enabled(),
            manager.deshred_transaction_notifications_enabled(),
            manager.deshred_transaction_alt_resolution_enabled(),
        ]
    }

    #[test]
    fn test_push_and_remove_plugin() {
        let mut manager = GeyserPluginManager::default();
        assert!(manager.plugins().is_empty());

        manager.push_plugin(test_plugin("first", false, false));
        manager.push_plugin(test_plugin("second", false, false));
        assert_eq!(
            manager
                .plugins()
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );

        let removed = manager.remove_plugin(0);
        assert_eq!(removed.name(), "first");
        assert_eq!(
            manager
                .plugins()
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>(),
            ["second"]
        );
    }

    #[test]
    fn test_from_plugins_preserves_order() {
        let manager = GeyserPluginManager::from_plugins(vec![
            Arc::new(test_plugin("a", false, false)),
            Arc::new(test_plugin("b", false, false)),
        ]);
        assert_eq!(
            manager
                .plugins()
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    /// A notification kind is enabled when *any* loaded plugin asks for it, and each of the
    /// seven kinds is answered independently of the others.
    #[test]
    fn test_notifications_enabled_is_per_kind_and_any_plugin() {
        let manager = GeyserPluginManager::default();
        assert_eq!(enabled_kinds(&manager), [false; 7]);

        let setters: [fn(&mut Kinds); 7] = [
            |k| k.accounts = true,
            |k| k.snapshot = true,
            |k| k.transactions = true,
            |k| k.entries = true,
            |k| k.block_footer = true,
            |k| k.deshred = true,
            |k| k.deshred_alt = true,
        ];
        for (i, set) in setters.iter().enumerate() {
            let mut kinds = Kinds::default();
            set(&mut kinds);
            let mut manager = GeyserPluginManager::default();
            // A plugin that wants nothing must not flip any fan-in.
            manager.push_plugin(loaded(TestPlugin {
                name: "quiet",
                kinds: Kinds::default(),
            }));
            manager.push_plugin(loaded(TestPlugin {
                name: "one-kind",
                kinds,
            }));
            let mut expected = [false; 7];
            expected[i] = true;
            assert_eq!(enabled_kinds(&manager), expected, "kind index {i}");
        }
    }

    /// `unload_plugin_blocking` must not return while another reference to the plugin is
    /// alive; it returns once that reference is dropped.
    #[test]
    fn test_unload_plugin_blocking_waits_for_outstanding_references() {
        let plugin = Arc::new(test_plugin("held", false, false));
        let held = Arc::clone(&plugin);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_flag = Arc::clone(&released);
        let holder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            released_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(held);
        });

        GeyserPluginManager::unload_plugin_blocking(plugin, 0);
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "unload returned before the outstanding reference was released"
        );
        holder.join().unwrap();
    }

    #[test]
    fn test_unload_drops_every_plugin() {
        let mut manager = GeyserPluginManager::default();
        manager.push_plugin(test_plugin("first", false, false));
        manager.push_plugin(test_plugin("second", false, false));

        manager.unload();
        assert!(manager.plugins().is_empty());
    }

    /// Writes a config file into a fresh temp directory and returns its path.
    fn write_config(name: &str, contents: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("geyser-host-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn test_load_plugin_from_config_rejects_invalid_json5() {
        let path = write_config("invalid-json", "{ libpath: ");
        let err = load_plugin_from_config(&path).expect_err("malformed config must not load");
        assert!(matches!(
            err,
            GeyserPluginManagerError::InvalidConfigFileFormat(_)
        ));
    }

    #[test]
    fn test_load_plugin_from_config_requires_libpath() {
        let path = write_config("no-libpath", r#"{ "name": "no-lib" }"#);
        let err = load_plugin_from_config(&path).expect_err("config without libpath must not load");
        assert!(matches!(err, GeyserPluginManagerError::LibPathNotSet));
    }

    /// A relative `libpath` is resolved against the config file's directory, and a library
    /// that cannot be opened surfaces as a load error rather than a panic.
    #[test]
    fn test_load_plugin_from_config_resolves_relative_libpath_and_reports_load_errors() {
        let path = write_config(
            "missing-lib",
            r#"{ "libpath": "does-not-exist.so", "name": "ghost" }"#,
        );
        let err = load_plugin_from_config(&path).expect_err("a missing library must not load");
        assert!(matches!(err, GeyserPluginManagerError::PluginLoadError(_)));
    }

    #[test]
    fn test_load_plugin_from_config_rejects_missing_file() {
        let err = load_plugin_from_config(Path::new("/definitely/not/a/geyser/config.json"))
            .expect_err("a missing config file must not load a plugin");
        assert!(matches!(
            err,
            GeyserPluginManagerError::CannotOpenConfigFile(_)
        ));
    }
}
