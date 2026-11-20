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
    file::{ConfigDocument, DEFAULT_CONFIG, load, parse},
    interface::{NetworkDeviceSelector, NetworkInterface, RouteSelector},
    xdp::{
        EndpointTx, QueueSelection, XdpEndpoints, XdpRuntimeConfig, XdpWorkerBinding, XdpWorkerPool,
    },
};
use {
    serde::Deserialize,
    std::collections::BTreeMap,
    xdp::{ConfigXdp, EndpointXdp},
};

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
    xdp_queue_ids: Option<XdpEndpoints<Vec<u32>>>,
}

impl ValidatedConfig {
    /// Whether XDP is enabled for all endpoints.
    pub fn xdp_enabled(&self) -> bool {
        self.xdp_queue_ids.is_some()
    }

    /// Effective network interfaces by logical label, including when XDP is disabled.
    pub fn interfaces(&self) -> &BTreeMap<String, NetworkInterface> {
        &self.effective.interfaces
    }

    /// Effective XDP transmit settings for each endpoint, including when XDP is disabled.
    pub fn xdp_endpoints(&self) -> XdpEndpoints<&EndpointTx> {
        self.effective.endpoints().map(|&endpoint| &endpoint.tx)
    }

    /// Selected hardware queue IDs in each endpoint's sender order.
    /// Returns `None` when XDP is disabled.
    pub fn xdp_queue_ids(&self) -> Option<XdpEndpoints<&[u32]>> {
        self.xdp_queue_ids
            .as_ref()
            .map(|queues| queues.map(Vec::as_slice))
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

#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;
