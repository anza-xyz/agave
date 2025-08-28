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
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
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

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        dirs_next::config_dir().map(|mut path| {
            path.extend(["agave", "validator.toml"]);
            path
        })
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let file = fs::read_to_string(path)?;
        let config = toml::from_str(&file)?;
        Ok(config)
    }
}
