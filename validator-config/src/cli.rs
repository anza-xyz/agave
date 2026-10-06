//! Applying CLI overrides to the file policy while retaining provenance.

use crate::{
    DeviceSelector, EffectiveConfig, Source,
    interface::interface_path,
    xdp::{WorkerPolicy, validate_pool_len, validate_unique_cpus},
};

#[derive(Clone, Debug, Default)]
pub struct CliOverrides {
    pub no_xdp: bool,
    pub interface: Option<String>,
    pub cpu_cores: Option<Vec<usize>>,
    pub zero_copy: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct CliApplication {
    pub config: EffectiveConfig,
    pub warnings: Vec<String>,
}

pub fn apply_cli(
    mut config: EffectiveConfig,
    overrides: CliOverrides,
) -> Result<CliApplication, String> {
    if overrides.no_xdp {
        config.xdp.enabled = false;
    }
    let mut warnings = Vec::new();
    if !config.xdp_active() {
        if let Some(interface) = overrides.interface {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring --xdp-interface={interface}"
            ));
        }
        if let Some(cpus) = overrides.cpu_cores {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring --xdp-cpu-cores={}",
                cpus.iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if let Some(zero_copy) = overrides.zero_copy {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring {}",
                if zero_copy {
                    "--xdp-zero-copy"
                } else {
                    "--no-xdp-zero-copy"
                }
            ));
        }
        warnings.sort();
        return Ok(CliApplication { config, warnings });
    }
    if config.interfaces.len() != 1 {
        return Err(format!(
            "XDP currently supports exactly one effective interface; found {}",
            config.interfaces.len()
        ));
    }
    let (label, interface) = config
        .interfaces
        .iter_mut()
        .next()
        .expect("XDP config should contain exactly one interface after validation");
    if let Some(cpus) = overrides.cpu_cores {
        validate_pool_len(cpus.len(), "--xdp-cpu-cores")?;
        validate_unique_cpus(&cpus, "--xdp-cpu-cores")?;
        if interface.xdp.workers_source == Source::User {
            warnings.push(format!(
                "--xdp-cpu-cores replaces user-authored workers for interface {label:?}"
            ));
        }
        interface.xdp.workers = WorkerPolicy::Cpus(cpus);
        interface.xdp.workers_source = Source::Cli;
    }
    if let Some(name) = overrides.interface {
        let same = matches!(&interface.device, DeviceSelector::Name(old) if old == &name);
        if matches!(&interface.xdp.workers, WorkerPolicy::Bindings(_)) && !same {
            return Err(format!(
                "--xdp-interface changes the device while workers.bindings is active for \
                 interface {label:?}; also supply --xdp-cpu-cores or update workers.bindings"
            ));
        }
        if !same {
            if interface.device_source == Source::User {
                warnings.push(format!(
                    "--xdp-interface replaces the user-authored device selector for interface \
                     {label:?}"
                ));
            }
            interface.device = DeviceSelector::Name(name);
            interface.device_source = Source::Cli;
        }
    }
    if let Some(zero_copy) = overrides.zero_copy {
        if interface.xdp.zero_copy_source == Source::User {
            warnings.push(format!(
                "{} replaces user-authored {}.xdp.zero_copy",
                if zero_copy {
                    "--xdp-zero-copy"
                } else {
                    "--no-xdp-zero-copy"
                },
                interface_path(label)
            ));
        }
        interface.xdp.zero_copy = zero_copy;
        interface.xdp.zero_copy_source = Source::Cli;
    }
    Ok(CliApplication { config, warnings })
}
