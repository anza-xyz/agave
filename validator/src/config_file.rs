//! Validator configuration file.
//!
//! The validator configuration file is a TOML file that can be used to configure CLI argument defaults.
//! The default path is `~/.config/agave/validator.toml`.
//!
//! # Goals
//!
//! - Reduce the number of CLI arguments needed when starting the validator.
//! - Explicitly passed CLI arguments should take precedence over config file values.
//! - Should be minimally invasive to existing CLI argument code.
//!     - Config values should plug in directly as default values to CLI arguments.
//!     - The configuration's rust defintions should generally be `Option`al or provide a sane default.

use {
    serde::Deserialize,
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

#[derive(Debug, Deserialize, Default)]
pub struct Xdp {
    pub interface: Option<String>,
    pub cpus: Option<String>,
    pub zero_copy: Option<bool>,
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
