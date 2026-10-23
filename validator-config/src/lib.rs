#![cfg(feature = "agave-unstable-api")]

//! Validator configuration schema, file loading, and policy validation.
//! XDP policy resolution uses supplied CPU information; device lookup and
//! transmitter setup belong to the validator.

mod file;
mod interface;
mod xdp;

pub use {
    file::DEFAULT_CONFIG,
    interface::{NetworkDeviceSelector, RouteSelector},
    xdp::{XdpEndpoints, XdpPolicy, XdpRuntimeConfig, XdpWorkerBinding},
};
use {
    interface::NetworkInterface,
    serde::Deserialize,
    std::{collections::BTreeMap, path::Path},
    xdp::{ConfigXdp, EndpointXdp},
};

/// An endpoint's section, such as `[tpu]`, after merging the user config file over
/// the built-in defaults, before validation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveEndpoint {
    xdp: EndpointXdp,
}

/// The built-in defaults merged with the user config file, before validation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveConfig {
    xdp: ConfigXdp,
    interfaces: BTreeMap<String, NetworkInterface>,
    gossip: EffectiveEndpoint,
    repair: EffectiveEndpoint,
    tpu: EffectiveEndpoint,
    turbine: EffectiveEndpoint,
    votor: EffectiveEndpoint,
}

/// A config that passed validation in `load`
#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    xdp: Option<XdpPolicy>,
}

impl ValidatedConfig {
    /// The XDP policy, or `None` when XDP is disabled.
    pub fn xdp(&self) -> Option<&XdpPolicy> {
        self.xdp.as_ref()
    }
}

impl EffectiveConfig {
    fn endpoints(&self) -> XdpEndpoints<&EffectiveEndpoint> {
        XdpEndpoints {
            gossip: &self.gossip,
            repair: &self.repair,
            tpu: &self.tpu,
            turbine: &self.turbine,
            votor: &self.votor,
        }
    }
}

/// Load the built-in defaults merged with the user config file and validate the result.
pub fn load(user_path: Option<&Path>) -> Result<(ValidatedConfig, Vec<String>), String> {
    let description = user_path.map_or_else(
        || "<built-in>".to_string(),
        |path| path.display().to_string(),
    );
    let config = file::read_config(user_path, &description)?;
    let (xdp, warnings) = xdp::validate(&config)
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    Ok((ValidatedConfig { xdp }, warnings))
}

#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;
