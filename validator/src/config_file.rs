//! Validator configuration file.
//!
//! The validator configuration file is a TOML file that can be used to configure CLI argument defaults.
//! The default path is `~/.config/agave/validator.toml`.
use {
    serde::{de::Deserializer, Deserialize},
    solana_clap_utils::input_parsers::parse_cpu_ranges,
    std::{
        fs::{self},
        path::{Path, PathBuf},
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum ValidatorConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

#[derive(Debug, Deserialize, Default)]
pub struct ValidatorConfig {
    #[serde(default)]
    pub net: Net,
}

#[derive(Debug, Deserialize, Default)]
pub struct Net {
    #[serde(default)]
    pub xdp: Xdp,
}

#[derive(Debug)]
pub struct XdpCpuRanges(pub Vec<usize>);

impl<'de> serde::de::Deserialize<'de> for XdpCpuRanges {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_cpu_ranges(&s)
            .map(XdpCpuRanges)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct Xdp {
    pub interface: Option<Vec<XdpInterface>>,
    pub cpus: Option<XdpCpuRanges>,
    pub zero_copy: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct XdpInterface {
    pub interface: String,
    pub queue: u64,
}

impl ValidatorConfig {
    pub fn default_path() -> Option<PathBuf> {
        dirs_next::config_dir().map(|mut path| {
            path.extend(["agave", "validator.toml"]);
            path
        })
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ValidatorConfigError> {
        let file = fs::read_to_string(path)?;
        let config: ValidatorConfig = toml::from_str(&file)?;
        Ok(config)
    }

    pub fn load_from_default_path() -> ValidatorConfig {
        match ValidatorConfig::default_path() {
            Some(config_path) => ValidatorConfig::from_path(config_path)
                .inspect_err(|e| match e {
                    ValidatorConfigError::Io(e) => {
                        log::warn!("Unable to read validator config file: {e}");
                    }
                    ValidatorConfigError::Toml(e) => {
                        log::warn!("Error parsing validator config file: {e}");
                    }
                })
                .unwrap_or_default(),
            None => {
                log::warn!("Unable to determine system config path for validator config");
                ValidatorConfig::default()
            }
        }
    }
}
