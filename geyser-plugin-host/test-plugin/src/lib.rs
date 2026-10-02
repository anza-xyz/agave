use {
    agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, Result},
    std::path::PathBuf,
};

#[derive(Debug, Default)]
struct TestPlugin {
    config_file: Option<PathBuf>,
}

impl GeyserPlugin for TestPlugin {
    fn name(&self) -> &'static str {
        "host-test-plugin"
    }

    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> Result<()> {
        self.config_file = Some(config_file.into());
        Ok(())
    }

    fn transaction_notifications_enabled(&self) -> bool {
        self.config_file.is_some()
    }

    fn on_unload(&mut self) {
        let path = self
            .config_file
            .as_ref()
            .unwrap()
            .with_extension("unloaded");
        std::fs::write(path, "unloaded").unwrap();
    }
}

// Match the constructor type used by load_plugin_from_config.
#[unsafe(no_mangle)]
pub fn _create_plugin() -> *mut dyn GeyserPlugin {
    Box::into_raw(Box::<TestPlugin>::default())
}
