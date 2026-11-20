#![cfg(feature = "agave-unstable-api")]

//! Validator configuration schema, TOML parsing, and policy validation.
//! XDP policy resolution uses supplied CPU information; device lookup and
//! transmitter setup belong to the validator.

mod diagnostic;
mod file;
mod interface;
mod xdp;

pub use {
    diagnostic::{Diagnostic, DiagnosticKind, FieldPathSegment},
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
pub fn load(user_path: Option<&Path>) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let Some(path) = user_path else {
        return parse_with_description(None, "<built-in>");
    };
    let description = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|error| {
        Diagnostic::new(DiagnosticKind::Io, error.to_string()).in_source(&description)
    })?;
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
/// # Ok::<(), agave_validator_config::Diagnostic>(())
/// ```
pub fn parse(text: &str) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    parse_with_description(Some(text), INPUT_DESCRIPTION)
}

fn parse_with_description(
    user_text: Option<&str>,
    description: &str,
) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let document = user_text
        .map(|text| ConfigDocument::parse_with_description(text, description))
        .transpose()?;
    validate_document(document, description)
}

fn validate_document(
    document: Option<ConfigDocument>,
    description: &str,
) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let source_text = document
        .as_ref()
        .and_then(|document| document.source_text.clone());
    let source_text = if document.is_none() {
        Some(DEFAULT_CONFIG)
    } else {
        source_text.as_deref()
    };
    let result = file::merge_config(document, description).and_then(|effective| {
        let (xdp, warnings) = xdp::validate(&effective)?;
        Ok((ValidatedConfig { effective, xdp }, warnings))
    });
    match result {
        Ok((config, mut warnings)) => {
            file::attach_source_locations(&mut warnings, description, source_text);
            Ok((config, warnings))
        }
        Err(mut error) => {
            file::attach_source_locations(
                std::slice::from_mut(&mut error),
                description,
                source_text,
            );
            Err(error)
        }
    }
}

#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;
