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
    interface::{EffectiveInterface, interface_path},
    serde::Deserialize,
    std::collections::BTreeMap,
    xdp::{ComponentXdp, GlobalXdp, WorkerPolicy},
};

const SCHEMA_VERSION: i64 = 1;

fn validate_schema_version(version: i64) -> Result<(), String> {
    if version != SCHEMA_VERSION {
        return Err(format!(
            "schema_version {version} is unsupported; this binary supports version \
             {SCHEMA_VERSION}"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Source {
    #[default]
    BuiltIn,
    User,
    Cli,
}

#[derive(Clone, Debug)]
pub struct Components<T> {
    pub gossip: T,
    pub repair: T,
    pub tpu: T,
    pub turbine: T,
    pub votor: T,
}

impl<T> Components<T> {
    fn values(&self) -> [&T; 5] {
        [
            &self.gossip,
            &self.repair,
            &self.tpu,
            &self.turbine,
            &self.votor,
        ]
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveComponent {
    xdp: ComponentXdp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfig {
    schema_version: i64,
    xdp: GlobalXdp,
    interfaces: BTreeMap<String, EffectiveInterface>,
    gossip: EffectiveComponent,
    repair: EffectiveComponent,
    tpu: EffectiveComponent,
    turbine: EffectiveComponent,
    votor: EffectiveComponent,
}

impl EffectiveConfig {
    pub fn xdp_active(&self) -> bool {
        self.xdp.enabled
    }

    /// Validate schema constraints independently of CLI overrides and host state,
    /// even when XDP is disabled. Deserialization alone does not check them.
    pub fn validate_structural(&self) -> Result<(), String> {
        validate_schema_version(self.schema_version)?;
        for (label, interface) in &self.interfaces {
            interface
                .xdp
                .workers
                .validate()
                .map_err(|error| format!("{}.xdp.workers: {error}", interface_path(label)))?;
            if matches!(interface.xdp.workers, WorkerPolicy::Bindings(_))
                && !matches!(interface.device, DeviceSelector::Name(_))
            {
                return Err(format!(
                    "{}.xdp.workers.bindings requires device.name; use workers.auto/workers.cpus \
                     with device.route, or name the device",
                    interface_path(label)
                ));
            }
        }
        for (name, component) in self.named_components() {
            component
                .tx
                .queues
                .validate()
                .map_err(|error| format!("{name}.xdp.tx.queues: {error}"))?;
        }
        Ok(())
    }

    fn named_components(&self) -> [(&'static str, &ComponentXdp); 5] {
        [
            ("gossip", &self.gossip.xdp),
            ("repair", &self.repair.xdp),
            ("tpu", &self.tpu.xdp),
            ("turbine", &self.turbine.xdp),
            ("votor", &self.votor.xdp),
        ]
    }
}

#[cfg(test)]
mod tests;
