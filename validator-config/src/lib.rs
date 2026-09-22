#![cfg(feature = "agave-unstable-api")]

//! Validator configuration schema, file loading, and policy validation.
//! CLI overrides retain their provenance. XDP policy resolution uses supplied
//! CPU information; device lookup and transmitter setup belong to the validator.

mod cli;
mod file;
pub mod interface;
pub mod xdp;

pub use {
    cli::{CliApplication, CliOverrides, apply_cli},
    file::{DEFAULT_CONFIG, load},
    interface::{DeviceSelector, RouteSelector},
    xdp::{QueueCpuBinding, RuntimeXdpConfig, resolve_runtime, validate_policy},
};
use {
    interface::EffectiveInterface,
    serde::Deserialize,
    std::collections::BTreeMap,
    xdp::{GlobalXdp, ModuleXdp},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Source {
    #[default]
    BuiltIn,
    User,
    Cli,
}

#[derive(Clone, Debug)]
pub struct Modules<T> {
    pub gossip: T,
    pub repair: T,
    pub tpu: T,
    pub turbine: T,
}

impl<T> Modules<T> {
    fn values(&self) -> [&T; 4] {
        [&self.gossip, &self.repair, &self.tpu, &self.turbine]
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveModule {
    xdp: ModuleXdp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfig {
    // Validated against SCHEMA_VERSION on the raw TOML before decoding; carried
    // only so deny_unknown_fields accepts the key.
    #[serde(rename = "schema_version")]
    _schema_version: i64,
    xdp: GlobalXdp,
    interfaces: BTreeMap<String, EffectiveInterface>,
    gossip: EffectiveModule,
    repair: EffectiveModule,
    tpu: EffectiveModule,
    turbine: EffectiveModule,
}

impl EffectiveConfig {
    pub fn xdp_active(&self) -> bool {
        self.xdp.enabled
    }

    fn named_modules(&self) -> [(&'static str, &ModuleXdp); 4] {
        [
            ("gossip", &self.gossip.xdp),
            ("repair", &self.repair.xdp),
            ("tpu", &self.tpu.xdp),
            ("turbine", &self.turbine.xdp),
        ]
    }
}

#[cfg(test)]
mod tests;
