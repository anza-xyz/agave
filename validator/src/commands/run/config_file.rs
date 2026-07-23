//! Parser for the validator's `--experimental-config-file` TOML file.
//!
//! Configuration is resolved in three layers, each overriding the previous:
//!   1. the built-in defaults, embedded from `default_config.toml`;
//!   2. the user file passed with `--experimental-config-file`, which overrides
//!      the default file section by section;
//!   3. CLI flags, applied by the caller (see `execute.rs`).
//!
//! ```toml
//! [interfaces."ens1f0"]
//! zero_copy = true
//! queue_to_cpu_mapping = [
//!   { queue = 0, cpu = 8 },
//!   { queue = 1, cpu = 9 },
//! ]
//!
//! [turbine]
//! use_xdp = true
//! [turbine.xdp]
//! tx = { ens1f0 = [0, 1] }
//! ```
//!
//! `[interfaces.<nic>]` declares a NIC's hardware queue -> CPU worker pool. Each
//! XDP-capable module (`tpu`, `turbine`, `repair`, `gossip`) may enable XDP
//! transmit (`use_xdp`) and, under `[<module>.xdp].tx`, name the interface(s)
//! and queues it transmits over. Unknown fields and unknown top-level sections
//! are rejected so typos fail loudly.
//!
//! The runtime currently drives a single shared XDP transmitter, so the enabled
//! modules' `tx` maps must reference at most one interface; the transmitter is
//! built over the union of the queues they name. A module that enables XDP
//! without naming any queue (as in the built-in default) leaves the interface
//! and CPU unspecified, and the caller falls back to auto-detecting the
//! default-route interface and auto-selecting a CPU.

use {
    agave_xdp::transmitter::QueueCpuBinding,
    serde::Deserialize,
    std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    },
};

/// The built-in defaults, embedded into the binary.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

fn default_true() -> bool {
    true
}

/// TOML shape used for serde. Field-typed rather than a map so that unknown
/// module/section names are rejected by `deny_unknown_fields`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    interfaces: BTreeMap<String, RawInterface>,
    tpu: Option<RawModule>,
    turbine: Option<RawModule>,
    repair: Option<RawModule>,
    gossip: Option<RawModule>,
}

impl RawConfig {
    /// The XDP-capable module blocks paired with their names.
    fn modules(&self) -> [(&'static str, &Option<RawModule>); 4] {
        [
            ("tpu", &self.tpu),
            ("turbine", &self.turbine),
            ("repair", &self.repair),
            ("gossip", &self.gossip),
        ]
    }
}

/// A single `[interfaces.<nic>]` entry: a NIC's queue -> CPU worker pool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInterface {
    #[serde(default)]
    zero_copy: bool,
    queue_to_cpu_mapping: Vec<QueueCpu>,
}

/// One `{ queue = <n>, cpu = <n> }` binding.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueCpu {
    queue: u32,
    cpu: usize,
}

/// A single `[<module>]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModule {
    /// Whether the module transmits over XDP. Defaults to `true` so that a user
    /// file adding only a `[<module>.xdp]` block does not silently disable the
    /// module.
    #[serde(default = "default_true")]
    use_xdp: bool,
    xdp: Option<RawModuleXdp>,
}

/// The `[<module>.xdp]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModuleXdp {
    /// Transmit queues per interface: `{ <nic> = [<queue>, ...] }`.
    #[serde(default)]
    tx: BTreeMap<String, Vec<u32>>,
}

/// The XDP inputs distilled from the merged config, before CLI overrides. The
/// caller (see `execute.rs`) layers CLI flags on top and, when `interface` or
/// `queues` are unset, applies auto-detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XdpFileConfig {
    /// Whether any module enabled XDP transmit.
    pub enabled: bool,
    /// The single interface referenced by enabled modules' `tx` maps, if any.
    /// `None` means no interface was named; the caller auto-detects.
    pub interface: Option<String>,
    /// Union of the enabled modules' `tx` queues, mapped to CPUs via the
    /// interface's `queue_to_cpu_mapping`. Empty means no queues were named;
    /// the caller auto-selects a CPU.
    pub queues: Vec<QueueCpuBinding>,
    /// Zero-copy setting of the referenced interface (`false` when none).
    pub zero_copy: bool,
}

/// Resolve the effective XDP configuration from the built-in defaults overlaid
/// with the optional user file.
pub(crate) fn resolve_xdp_config(user_path: Option<&Path>) -> Result<XdpFileConfig, String> {
    let base = parse_str(DEFAULT_CONFIG)
        .map_err(|e| format!("built-in default config is invalid: {e}"))?;
    let merged = match user_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read config file `{}`: {e}", path.display()))?;
            let over = parse_str(&text)
                .map_err(|e| format!("invalid config file `{}`: {e}", path.display()))?;
            merge(base, over)
        }
        None => base,
    };
    resolve(&merged)
}

fn parse_str(text: &str) -> Result<RawConfig, toml::de::Error> {
    toml::from_str(text)
}

/// Overlay `over` onto `base`: interfaces merge per NIC name, and a module block
/// present in `over` replaces the base block wholesale.
fn merge(mut base: RawConfig, over: RawConfig) -> RawConfig {
    base.interfaces.extend(over.interfaces);
    if over.tpu.is_some() {
        base.tpu = over.tpu;
    }
    if over.turbine.is_some() {
        base.turbine = over.turbine;
    }
    if over.repair.is_some() {
        base.repair = over.repair;
    }
    if over.gossip.is_some() {
        base.gossip = over.gossip;
    }
    base
}

/// A validated `[interfaces.<nic>]` entry.
struct ResolvedInterface {
    zero_copy: bool,
    queue_to_cpu: BTreeMap<u32, usize>,
}

fn resolve_interface(name: &str, raw: &RawInterface) -> Result<ResolvedInterface, String> {
    if raw.queue_to_cpu_mapping.is_empty() {
        return Err(format!("interface `{name}` has an empty queue_to_cpu_mapping"));
    }
    let mut queue_to_cpu = BTreeMap::new();
    let mut seen_cpus = BTreeSet::new();
    for QueueCpu { queue, cpu } in &raw.queue_to_cpu_mapping {
        if queue_to_cpu.insert(*queue, *cpu).is_some() {
            return Err(format!(
                "interface `{name}` maps queue {queue} more than once"
            ));
        }
        if !seen_cpus.insert(*cpu) {
            return Err(format!(
                "interface `{name}` maps CPU {cpu} to more than one queue"
            ));
        }
    }
    Ok(ResolvedInterface {
        zero_copy: raw.zero_copy,
        queue_to_cpu,
    })
}

fn resolve(config: &RawConfig) -> Result<XdpFileConfig, String> {
    // Validate every declared interface's queue -> CPU mapping up front.
    let interfaces = config
        .interfaces
        .iter()
        .map(|(name, raw)| resolve_interface(name, raw).map(|iface| (name.clone(), iface)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let mut enabled = false;
    let mut referenced_iface: Option<String> = None;
    let mut used_queues: BTreeSet<u32> = BTreeSet::new();

    for (module, block) in config.modules() {
        // A module absent post-merge keeps XDP on, matching the default file.
        let use_xdp = block.as_ref().map(|b| b.use_xdp).unwrap_or(true);
        if !use_xdp {
            continue;
        }
        enabled = true;

        let Some(tx) = block.as_ref().and_then(|b| b.xdp.as_ref()).map(|x| &x.tx) else {
            continue;
        };
        for (iface_name, queues) in tx {
            let iface = interfaces.get(iface_name).ok_or_else(|| {
                format!(
                    "module `{module}` XDP tx references undeclared interface `{iface_name}`; add \
                     an [interfaces.\"{iface_name}\"] section"
                )
            })?;
            if queues.is_empty() {
                return Err(format!(
                    "module `{module}` XDP tx for interface `{iface_name}` lists no queues"
                ));
            }
            match &referenced_iface {
                Some(existing) if existing != iface_name => {
                    return Err(format!(
                        "XDP tx references multiple interfaces (`{existing}` and `{iface_name}`); \
                         only one interface is supported"
                    ));
                }
                Some(_) => {}
                None => referenced_iface = Some(iface_name.clone()),
            }
            for queue in queues {
                if !iface.queue_to_cpu.contains_key(queue) {
                    return Err(format!(
                        "module `{module}` XDP tx references queue {queue} on `{iface_name}`, \
                         which is not in its queue_to_cpu_mapping"
                    ));
                }
                used_queues.insert(*queue);
            }
        }
    }

    let (interface, queues, zero_copy) = match referenced_iface {
        Some(name) => {
            let iface = &interfaces[&name];
            let queues = used_queues
                .iter()
                .map(|queue| QueueCpuBinding {
                    queue: *queue,
                    cpu: iface.queue_to_cpu[queue],
                })
                .collect();
            (Some(name), queues, iface.zero_copy)
        }
        None => (None, Vec::new(), false),
    };

    Ok(XdpFileConfig {
        enabled,
        interface,
        queues,
        zero_copy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Merge the built-in defaults with `user` TOML and resolve, mirroring
    /// `resolve_xdp_config` without touching the filesystem.
    fn resolve_with_user(user: &str) -> Result<XdpFileConfig, String> {
        let base = parse_str(DEFAULT_CONFIG).expect("default config parses");
        let over = parse_str(user).map_err(|e| e.to_string())?;
        resolve(&merge(base, over))
    }

    #[test]
    fn default_config_enables_all_modules_with_auto_selection() {
        let base = parse_str(DEFAULT_CONFIG).expect("default config parses");
        let c = resolve(&base).unwrap();
        assert!(c.enabled);
        assert_eq!(c.interface, None);
        assert!(c.queues.is_empty());
        assert!(!c.zero_copy);
    }

    #[test]
    fn interface_and_tx_resolve_to_queue_bindings() {
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nzero_copy = true\nqueue_to_cpu_mapping = [{ queue = 0, cpu = \
             8 }, { queue = 1, cpu = 9 }]\n\n[turbine.xdp]\ntx = { eth0 = [0, 1] }\n",
        )
        .unwrap();
        assert!(c.enabled);
        assert_eq!(c.interface.as_deref(), Some("eth0"));
        assert!(c.zero_copy);
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 0, cpu: 8 },
                QueueCpuBinding { queue: 1, cpu: 9 },
            ]
        );
    }

    #[test]
    fn union_of_module_queues_is_taken() {
        // tpu uses queue 0, turbine queue 1; the shared transmitter needs both.
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }, { queue = 1, \
             cpu = 9 }]\n\n[tpu.xdp]\ntx = { eth0 = [0] }\n\n[turbine.xdp]\ntx = { eth0 = [1] }\n",
        )
        .unwrap();
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 0, cpu: 8 },
                QueueCpuBinding { queue: 1, cpu: 9 },
            ]
        );
    }

    #[test]
    fn adding_xdp_block_does_not_disable_module() {
        // A user file that adds only [turbine.xdp] must keep use_xdp = true.
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n\n\
             [turbine.xdp]\ntx = { eth0 = [0] }\n",
        )
        .unwrap();
        assert!(c.enabled);
        assert_eq!(c.interface.as_deref(), Some("eth0"));
    }

    #[test]
    fn all_modules_disabled_reports_not_enabled() {
        let c = resolve_with_user(
            "[tpu]\nuse_xdp = false\n[turbine]\nuse_xdp = false\n[repair]\nuse_xdp = false\n\
             [gossip]\nuse_xdp = false\n",
        )
        .unwrap();
        assert!(!c.enabled);
        assert_eq!(c.interface, None);
        assert!(c.queues.is_empty());
    }

    #[test]
    fn disabled_module_tx_is_ignored() {
        // turbine is off, so its (otherwise invalid) tx ref must not be checked.
        let c = resolve_with_user(
            "[turbine]\nuse_xdp = false\n[turbine.xdp]\ntx = { nosuchdev = [0] }\n",
        )
        .unwrap();
        assert_eq!(c.interface, None);
    }

    #[test]
    fn undeclared_interface_is_error() {
        let e = resolve_with_user("[turbine.xdp]\ntx = { eth0 = [0] }\n").unwrap_err();
        assert!(e.contains("undeclared interface"), "{e}");
    }

    #[test]
    fn queue_not_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n\n\
             [turbine.xdp]\ntx = { eth0 = [3] }\n",
        )
        .unwrap_err();
        assert!(e.contains("not in its queue_to_cpu_mapping"), "{e}");
    }

    #[test]
    fn multiple_interfaces_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n\
             [interfaces.\"eth1\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 9 }]\n\n\
             [tpu.xdp]\ntx = { eth0 = [0] }\n[turbine.xdp]\ntx = { eth1 = [0] }\n",
        )
        .unwrap_err();
        assert!(e.contains("multiple interfaces"), "{e}");
    }

    #[test]
    fn duplicate_queue_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }, { queue = 0, \
             cpu = 9 }]\n",
        )
        .unwrap_err();
        assert!(e.contains("queue 0 more than once"), "{e}");
    }

    #[test]
    fn duplicate_cpu_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }, { queue = 1, \
             cpu = 8 }]\n",
        )
        .unwrap_err();
        assert!(e.contains("CPU 8 to more than one queue"), "{e}");
    }

    #[test]
    fn empty_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = []\n\n[turbine.xdp]\ntx = { eth0 = \
             [0] }\n",
        )
        .unwrap_err();
        assert!(e.contains("empty queue_to_cpu_mapping"), "{e}");
    }

    #[test]
    fn empty_tx_queue_list_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n\n\
             [turbine.xdp]\ntx = { eth0 = [] }\n",
        )
        .unwrap_err();
        assert!(e.contains("lists no queues"), "{e}");
    }

    #[test]
    fn unknown_top_level_section_is_rejected() {
        let e = resolve_with_user("[nonsense]\nfoo = 1\n").unwrap_err();
        // serde reports an unknown-field error for the whole document.
        assert!(!e.is_empty());
        assert!(parse_str("[nonsense]\nfoo = 1\n").is_err());
    }

    #[test]
    fn unknown_module_field_is_rejected() {
        assert!(parse_str("[turbine]\nbogus = 1\n").is_err());
    }

    #[test]
    fn unknown_interface_field_is_rejected() {
        assert!(parse_str(
            "[interfaces.\"eth0\"]\nbogus = 1\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n"
        )
        .is_err());
    }
}
