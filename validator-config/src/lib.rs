#![cfg(feature = "agave-unstable-api")]

//! Validator configuration schema, TOML parsing, and policy validation.
//! XDP policy resolution uses supplied CPU information; device lookup and
//! transmitter setup belong to the validator.

mod file;
mod interface;
mod xdp;

pub use {
    file::{ConfigDocument, DEFAULT_CONFIG},
    interface::{NetworkDeviceSelector, NetworkInterface, RouteSelector},
    xdp::{
        EndpointTx, QueueSelection, XdpEndpoints, XdpPolicy, XdpRuntimeConfig, XdpWorkerBinding,
        XdpWorkerPool,
    },
};
use {
    serde::Deserialize,
    std::{collections::BTreeMap, path::Path},
    xdp::{ConfigXdp, EndpointXdp},
};

const INPUT_DESCRIPTION: &str = "<input>";

/// An endpoint's section, such as `[tpu]`, after merging the user config file over
/// the built-in defaults, before validation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveEndpoint {
    xdp: EndpointXdp,
}

/// The built-in defaults merged with the user config file.
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

/// Effective settings that passed host-independent validation.
#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    effective: EffectiveConfig,
    xdp: Option<XdpPolicy>,
}

impl ValidatedConfig {
    /// Whether XDP is enabled for all endpoints.
    pub fn xdp_enabled(&self) -> bool {
        self.effective.xdp.enabled
    }

    /// Effective network interfaces by logical label, including when XDP is disabled.
    pub fn interfaces(&self) -> &BTreeMap<String, NetworkInterface> {
        &self.effective.interfaces
    }

    /// Effective XDP transmit settings for each endpoint, including when XDP is disabled.
    pub fn xdp_endpoints(&self) -> XdpEndpoints<&EndpointTx> {
        self.effective.endpoints().map(|&endpoint| &endpoint.tx)
    }

    /// The XDP policy, or `None` when XDP is disabled.
    pub fn xdp(&self) -> Option<&XdpPolicy> {
        self.xdp.as_ref()
    }
}

impl EffectiveConfig {
    fn endpoints(&self) -> XdpEndpoints<&EndpointXdp> {
        XdpEndpoints {
            gossip: &self.gossip.xdp,
            repair: &self.repair.xdp,
            tpu: &self.tpu.xdp,
            turbine: &self.turbine.xdp,
            votor: &self.votor.xdp,
        }
    }
}

/// Load the built-in defaults merged with the user config file and validate the result.
pub fn load(user_path: Option<&Path>) -> Result<(ValidatedConfig, Vec<String>), String> {
    let Some(path) = user_path else {
        return parse_with_description(None, "<built-in>");
    };
    let description = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read config file `{description}`: {error}"))?;
    parse_with_description(Some(&text), &description)
}

/// Parse user TOML, merge it with the built-in defaults, and validate the result.
///
/// The input must include the supported `schema_version`. The returned list
/// contains any validation warnings.
///
/// ```
/// use agave_validator_config::parse;
///
/// let (config, warnings) = parse("schema_version = 1\nxdp.enabled = false")?;
/// assert!(config.xdp().is_none());
/// assert!(warnings.is_empty());
/// # Ok::<(), String>(())
/// ```
pub fn parse(text: &str) -> Result<(ValidatedConfig, Vec<String>), String> {
    parse_with_description(Some(text), INPUT_DESCRIPTION)
}

fn parse_with_description(
    user_text: Option<&str>,
    description: &str,
) -> Result<(ValidatedConfig, Vec<String>), String> {
    let document = user_text
        .map(|text| ConfigDocument::parse_with_description(text, description))
        .transpose()?;
    validate_document(document, description)
}

fn validate_document(
    document: Option<ConfigDocument>,
    description: &str,
) -> Result<(ValidatedConfig, Vec<String>), String> {
    let effective = file::merge_config(document, description)?;
    let (xdp, warnings) = xdp::validate(&effective)
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    Ok((ValidatedConfig { effective, xdp }, warnings))
}

#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;
