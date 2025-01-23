use {
    crate::{CoreAllocation, NativeConfig, RayonConfig, TokioConfig},
    serde::{Deserialize, Serialize},
    std::collections::HashMap,
    toml::de::Error,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadManagerConfig {
    /// Configurations for the native OS thread pools
    pub native_configs: HashMap<String, NativeConfig>,
    /// Mapping from lookup name to the config name for Native
    pub native_runtime_mapping: HashMap<String, String>,

    /// Configurations for the rayon runtimes
    pub rayon_configs: HashMap<String, RayonConfig>,
    /// Mapping from lookup name to the config name for Rayon
    pub rayon_runtime_mapping: HashMap<String, String>,

    /// Configurations for the Tokio runtimes
    pub tokio_configs: HashMap<String, TokioConfig>,
    /// Mapping from lookup name to the config name for Tokio
    pub tokio_runtime_mapping: HashMap<String, String>,

    pub default_core_allocation: CoreAllocation,
}

impl Default for ThreadManagerConfig {
    // Default is mostly to keep Deserialize implementation happy. This is not intended to be a valid config
    fn default() -> Self {
        Self {
            native_configs: HashMap::from([("default".to_owned(), NativeConfig::default())]),
            native_runtime_mapping: HashMap::new(),
            rayon_configs: HashMap::from([("default".to_owned(), RayonConfig::default())]),
            rayon_runtime_mapping: HashMap::new(),
            tokio_configs: HashMap::from([("default".to_owned(), TokioConfig::default())]),
            tokio_runtime_mapping: HashMap::new(),
            default_core_allocation: CoreAllocation::OsDefault,
        }
    }
}

impl ThreadManagerConfig {
    /// Returns a valid "built-in" configuration for Agave that is safe to use in most cases.
    /// Most operators will likely use this.
    pub fn default_for_agave() -> Self {
        let config_str = include_str!("default_config.toml");
        toml::from_str(config_str).expect("Parsing the built-in config should never fail")
    }

    /// Loads configuration from a given str, filling in the gaps from what default_for_agave returns
    pub fn from_toml_str(toml: &str) -> Result<Self, Error> {
        let mut loaded: ThreadManagerConfig = toml::from_str(toml)?;
        let mut result = Self::default_for_agave();
        macro_rules! upsert {
            ($name:ident) => {
                result.$name.extend(loaded.$name.drain());
            };
        }
        upsert!(native_configs);
        upsert!(native_runtime_mapping);
        upsert!(rayon_configs);
        upsert!(rayon_runtime_mapping);
        upsert!(tokio_configs);
        upsert!(tokio_runtime_mapping);
        result.default_core_allocation = loaded.default_core_allocation;
        Ok(result)
    }
}
