//! Parser for the validator's `--experimental-config-file` TOML file.
//!
//! Built-in defaults are merged with user TOML, then the caller applies matching
//! CLI overrides. The file currently covers XDP transmit settings.
//! `[interfaces.<nic>]` maps hardware queues to CPU workers, and
//! `[<module>.xdp].tx` selects which queues each XDP-enabled module uses.
//!
//! The runtime currently uses a single shared XDP transmitter, so enabled
//! modules may reference only one interface. Its queue set is the union of
//! enabled modules' `tx` queues; with no explicit queue, startup falls back to
//! interface/CPU auto-selection. An enabled module that names no `tx` transmits
//! over that whole shared queue set (the union above), not the NIC's full
//! hardware queue set. Every declared `[interfaces.<nic>]` must be named by some
//! module's `tx`; a dangling one is rejected.

use {
    agave_xdp::transmitter::QueueCpuBinding,
    log::warn,
    serde::Deserialize,
    std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    },
};

const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

fn default_true() -> bool {
    true
}

/// Field-typed TOML shape so `deny_unknown_fields` rejects unknown sections.
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInterface {
    #[serde(default)]
    zero_copy: bool,
    queue_to_cpu_mapping: Vec<QueueCpu>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueCpu {
    queue: u32,
    cpu: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModule {
    /// Defaults to true so a file that only adds `[<module>.xdp]` keeps XDP on.
    #[serde(default = "default_true")]
    use_xdp: bool,
    xdp: Option<RawModuleXdp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModuleXdp {
    /// Transmit queues per interface: `{ <nic> = [<queue>, ...] }`.
    #[serde(default)]
    tx: BTreeMap<String, Vec<u32>>,
}

/// A module's XDP transmit config: whether it uses XDP (`use_xdp`) and which of
/// `XdpFileConfig::queues` it transmits over, as positions into that list rather
/// than hardware queue ids. `tx_positions` is empty when an enabled module named
/// no queues, which the caller treats as "all queues". Disabled modules also
/// have an empty list but get no sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleXdp {
    pub enabled: bool,
    pub tx_positions: Box<[usize]>,
}

/// The XDP inputs distilled from the merged config, before CLI overrides. The
/// caller (see `execute.rs`) layers CLI flags on top and, when `interface` or
/// `queues` are unset, applies auto-detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XdpFileConfig {
    pub tpu: ModuleXdp,
    pub turbine: ModuleXdp,
    pub repair: ModuleXdp,
    pub gossip: ModuleXdp,
    /// The single interface referenced by enabled modules' `tx` maps, if any.
    /// `None` means no enabled module named an interface; the caller auto-detects.
    pub interface: Option<String>,
    /// Union of enabled modules' `tx` queues, mapped to CPUs via the interface's
    /// `queue_to_cpu_mapping`. Empty means no enabled module named queues; the
    /// caller auto-selects a CPU.
    pub queues: Vec<QueueCpuBinding>,
    /// Zero-copy setting of the referenced interface (`false` when none).
    pub zero_copy: bool,
}

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

/// Interfaces merge by NIC name; module blocks replace defaults wholesale.
fn merge(mut base: RawConfig, over: RawConfig) -> RawConfig {
    base.interfaces.extend(over.interfaces);
    base.tpu = over.tpu.or(base.tpu);
    base.turbine = over.turbine.or(base.turbine);
    base.repair = over.repair.or(base.repair);
    base.gossip = over.gossip.or(base.gossip);
    base
}

struct ResolvedInterface {
    zero_copy: bool,
    queue_to_cpu: BTreeMap<u32, usize>,
}

fn resolve_interface(name: &str, raw: &RawInterface) -> Result<ResolvedInterface, String> {
    if raw.queue_to_cpu_mapping.is_empty() {
        return Err(format!(
            "interface `{name}` has an empty queue_to_cpu_mapping"
        ));
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

/// A module's `tx` queue ids, before the union fixes their positions.
struct ResolvedModule {
    enabled: bool,
    tx_queues: BTreeSet<u32>,
}

/// A module absent post-merge keeps XDP on. Must agree with `default_true` on
/// `RawModule::use_xdp`, or a module's mere presence would change whether it
/// transmits over XDP.
fn module_enabled(block: &Option<RawModule>) -> bool {
    block.as_ref().is_none_or(|b| b.use_xdp)
}

/// Resolve one module's `use_xdp` and, when enabled, validated `tx` queue ids.
fn resolve_module(
    module: &str,
    block: &Option<RawModule>,
    interfaces: &BTreeMap<String, ResolvedInterface>,
) -> Result<ResolvedModule, String> {
    let enabled = module_enabled(block);
    if !enabled {
        return Ok(ResolvedModule {
            enabled: false,
            tx_queues: BTreeSet::new(),
        });
    }
    let mut tx_queues = BTreeSet::new();
    if let Some(tx) = block.as_ref().and_then(|b| b.xdp.as_ref()).map(|x| &x.tx) {
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
            for queue in queues {
                if !iface.queue_to_cpu.contains_key(queue) {
                    return Err(format!(
                        "module `{module}` XDP tx references queue {queue} on `{iface_name}`, \
                         which is not in its queue_to_cpu_mapping"
                    ));
                }
                if !tx_queues.insert(*queue) {
                    return Err(format!(
                        "module `{module}` XDP tx lists queue {queue} more than once"
                    ));
                }
            }
        }
    }
    Ok(ResolvedModule { enabled, tx_queues })
}

fn resolve(config: &RawConfig) -> Result<XdpFileConfig, String> {
    let interfaces = config
        .interfaces
        .iter()
        .map(|(name, raw)| resolve_interface(name, raw).map(|iface| (name.clone(), iface)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let blocks = [
        ("tpu", &config.tpu),
        ("turbine", &config.turbine),
        ("repair", &config.repair),
        ("gossip", &config.gossip),
    ];

    // Every declared interface must be named by some module's `tx` (whether or
    // not that module is enabled); a dangling `[interfaces.<name>]` is a typo or
    // leftover and is rejected rather than silently ignored.
    let tx_named: BTreeSet<&str> = blocks
        .into_iter()
        .filter_map(|(_, block)| block.as_ref())
        .filter_map(|m| m.xdp.as_ref())
        .flat_map(|x| x.tx.keys().map(String::as_str))
        .collect();
    for name in interfaces.keys() {
        if !tx_named.contains(name.as_str()) {
            return Err(format!(
                "interface `{name}` is declared but no module's XDP tx references it"
            ));
        }
    }

    // One shared transmitter means enabled modules' tx maps may name only one
    // interface. Disabled modules are exempt: their tx is never validated.
    let referenced: BTreeSet<&str> = blocks
        .into_iter()
        .filter(|(_, block)| module_enabled(block))
        .filter_map(|(_, block)| block.as_ref())
        .filter_map(|m| m.xdp.as_ref())
        .flat_map(|x| x.tx.keys().map(String::as_str))
        .collect();
    if referenced.len() > 1 {
        let names = referenced
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "XDP tx references multiple interfaces ({names}); only one interface is supported"
        ));
    }
    let referenced_iface = referenced.first().map(|name| name.to_string());

    let tpu = resolve_module("tpu", &config.tpu, &interfaces)?;
    let turbine = resolve_module("turbine", &config.turbine, &interfaces)?;
    let repair = resolve_module("repair", &config.repair, &interfaces)?;
    let gossip = resolve_module("gossip", &config.gossip, &interfaces)?;

    // An interface referenced only by disabled modules passes the dangling
    // check above but is dead at runtime; say so instead of silently falling
    // back to auto-selection.
    for name in interfaces.keys() {
        if Some(name) != referenced_iface.as_ref() {
            warn!(
                "interface `{name}` is only referenced by disabled modules; its configuration is \
                 unused"
            );
        }
    }

    // The transmitter's queue set is the union of enabled modules' tx queues.
    let used_queues: BTreeSet<u32> = [&tpu, &turbine, &repair, &gossip]
        .into_iter()
        .flat_map(|m| m.tx_queues.iter().copied())
        .collect();

    // An enabled module with no `tx` transmits over the whole union, so it shares
    // the queues other modules named for themselves. A config that looks like it
    // separates them does not, so say so rather than silently overlapping.
    let mut scoped = Vec::new();
    let mut unscoped = Vec::new();
    for (module, resolved) in [
        ("tpu", &tpu),
        ("turbine", &turbine),
        ("repair", &repair),
        ("gossip", &gossip),
    ] {
        if !resolved.enabled {
            continue;
        }
        if resolved.tx_queues.is_empty() {
            unscoped.push(module);
        } else {
            scoped.push(module);
        }
    }
    if !scoped.is_empty() && !unscoped.is_empty() {
        warn!(
            "modules {unscoped:?} name no XDP tx queues, so they transmit over every configured \
             queue, including the ones {scoped:?} named for themselves; give them their own `tx` \
             to keep the queue sets apart"
        );
    }

    // Queues are emitted in `used_queues` order, so a queue's rank in that set
    // is its position in `queues` and therefore in the transmitter's sender list.
    let position_of: BTreeMap<u32, usize> = used_queues
        .iter()
        .copied()
        .enumerate()
        .map(|(position, queue)| (queue, position))
        .collect();
    let to_module = |module: ResolvedModule| ModuleXdp {
        enabled: module.enabled,
        tx_positions: module
            .tx_queues
            .iter()
            .map(|queue| position_of[queue])
            .collect(),
    };

    let (interface, queues, zero_copy) = match referenced_iface {
        Some(name) => {
            let iface = &interfaces[&name];
            // A mapped queue no module named gets no transmit worker, which reads
            // as configured capacity that silently does not exist.
            let unused: Vec<u32> = iface
                .queue_to_cpu
                .keys()
                .copied()
                .filter(|queue| !used_queues.contains(queue))
                .collect();
            if !unused.is_empty() {
                warn!(
                    "interface `{name}` maps queues {unused:?} to CPUs but no module's XDP tx \
                     names them; they get no transmit worker"
                );
            }
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
        tpu: to_module(tpu),
        turbine: to_module(turbine),
        repair: to_module(repair),
        gossip: to_module(gossip),
        interface,
        queues,
        zero_copy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_with_user(user: &str) -> Result<XdpFileConfig, String> {
        let base = parse_str(DEFAULT_CONFIG).expect("default config parses");
        let over = parse_str(user).map_err(|e| e.to_string())?;
        resolve(&merge(base, over))
    }

    #[test]
    fn default_config_enables_all_modules_with_auto_selection() {
        let base = parse_str(DEFAULT_CONFIG).expect("default config parses");
        let c = resolve(&base).unwrap();
        assert!(c.tpu.enabled && c.turbine.enabled && c.repair.enabled && c.gossip.enabled);
        // No tx anywhere, so no queues are named; the caller auto-selects.
        assert!(c.turbine.tx_positions.is_empty());
        assert_eq!(c.interface, None);
        assert!(c.queues.is_empty());
        assert!(!c.zero_copy);
    }

    #[test]
    fn interfaces_merge_by_nic_name() {
        // `default_config.toml` declares no interfaces, so nothing reaches this
        // path through `resolve_xdp_config` yet. Exercise `merge` directly to keep
        // per-NIC merging working once the defaults do declare one.
        let base = parse_str(
            "[interfaces.\"eth0\"]\nzero_copy = true\nqueue_to_cpu_mapping = [{ queue = 0, cpu = \
             1 }]\n[interfaces.\"eth1\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 2 }]\n",
        )
        .unwrap();
        let over = parse_str(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 3, cpu = 9 \
             }]\n[interfaces.\"eth2\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 4 }]\n",
        )
        .unwrap();
        let merged = merge(base, over);

        // eth1 survives from the base and eth2 is added, so the maps merge rather
        // than the user file replacing the whole section.
        assert_eq!(
            merged
                .interfaces
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["eth0", "eth1", "eth2"]
        );
        // A NIC declared in both is replaced wholesale, dropping base zero_copy.
        let eth0 = &merged.interfaces["eth0"];
        assert!(!eth0.zero_copy);
        assert_eq!(eth0.queue_to_cpu_mapping.len(), 1);
        assert_eq!(eth0.queue_to_cpu_mapping[0].queue, 3);
        assert_eq!(eth0.queue_to_cpu_mapping[0].cpu, 9);
    }

    #[test]
    fn interface_and_tx_resolve_to_queue_bindings() {
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nzero_copy = true\nqueue_to_cpu_mapping = [{ queue = 0, cpu = \
             8 }, { queue = 1, cpu = 9 }]\n\n[turbine.xdp]\ntx = { eth0 = [0, 1] }\n",
        )
        .unwrap();
        assert_eq!(*c.turbine.tx_positions, [0, 1]);
        // Modules without a tx block name no queues.
        assert!(c.tpu.tx_positions.is_empty());
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
        // Positions index the union, so each module lands on the queue it named.
        assert_eq!(*c.tpu.tx_positions, [0]);
        assert_eq!(*c.turbine.tx_positions, [1]);
    }

    #[test]
    fn adding_xdp_block_does_not_disable_module() {
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n\n[turbine.xdp]\ntx = { eth0 = [0] }\n",
        )
        .unwrap();
        assert!(c.turbine.enabled);
        assert_eq!(*c.turbine.tx_positions, [0]);
        assert_eq!(c.interface.as_deref(), Some("eth0"));
    }

    #[test]
    fn all_modules_disabled_reports_not_enabled() {
        let c = resolve_with_user(
            "[tpu]\nuse_xdp = false\n[turbine]\nuse_xdp = false\n[repair]\nuse_xdp = \
             false\n[gossip]\nuse_xdp = false\n",
        )
        .unwrap();
        assert!(!c.tpu.enabled && !c.turbine.enabled && !c.repair.enabled && !c.gossip.enabled);
        assert_eq!(c.interface, None);
        assert!(c.queues.is_empty());
    }

    #[test]
    fn per_module_use_xdp_is_reported() {
        let c = resolve_with_user("[turbine]\nuse_xdp = false\n").unwrap();
        assert!(c.tpu.enabled);
        assert!(!c.turbine.enabled);
        assert!(c.repair.enabled);
        assert!(c.gossip.enabled);
    }

    #[test]
    fn disabled_module_tx_is_ignored() {
        // turbine is off, so its (otherwise invalid) tx ref must not be checked.
        let c = resolve_with_user(
            "[turbine]\nuse_xdp = false\n[turbine.xdp]\ntx = { nosuchdev = [0] }\n",
        )
        .unwrap();
        assert_eq!(c.interface, None);
        assert!(!c.turbine.enabled);
        assert!(c.turbine.tx_positions.is_empty());
    }

    #[test]
    fn undeclared_interface_is_error() {
        let e = resolve_with_user("[turbine.xdp]\ntx = { eth0 = [0] }\n").unwrap_err();
        assert!(e.contains("undeclared interface"), "{e}");
    }

    #[test]
    fn queue_not_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n\n[turbine.xdp]\ntx = { eth0 = [3] }\n",
        )
        .unwrap_err();
        assert!(e.contains("not in its queue_to_cpu_mapping"), "{e}");
    }

    #[test]
    fn multiple_interfaces_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n[interfaces.\"eth1\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 9 \
             }]\n\n[tpu.xdp]\ntx = { eth0 = [0] }\n[turbine.xdp]\ntx = { eth1 = [0] }\n",
        )
        .unwrap_err();
        assert!(e.contains("multiple interfaces"), "{e}");
    }

    #[test]
    fn unreferenced_interface_is_error() {
        // eth0 is declared with a valid mapping but no module names it in tx.
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n",
        )
        .unwrap_err();
        assert!(e.contains("declared but no module"), "{e}");
    }

    #[test]
    fn interface_referenced_only_by_disabled_module_is_ok() {
        // A disabled module's tx still keeps its interface from being treated
        // as an unreferenced declaration, although the interface is unused.
        let c = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n\n[turbine]\nuse_xdp = false\n[turbine.xdp]\ntx = { eth0 = [0] }\n",
        )
        .unwrap();
        assert!(!c.turbine.enabled);
    }

    #[test]
    fn duplicate_tx_queue_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n\n[turbine.xdp]\ntx = { eth0 = [0, 0] }\n",
        )
        .unwrap_err();
        // "lists" distinguishes a repeated tx entry from a repeated mapping entry.
        assert!(e.contains("lists queue 0 more than once"), "{e}");
    }

    #[test]
    fn duplicate_queue_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }, { queue = 0, \
             cpu = 9 }]\n\n[turbine.xdp]\ntx = { eth0 = [0] }\n",
        )
        .unwrap_err();
        assert!(e.contains("maps queue 0 more than once"), "{e}");
    }

    #[test]
    fn duplicate_cpu_in_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }, { queue = 1, \
             cpu = 8 }]\n\n[turbine.xdp]\ntx = { eth0 = [0] }\n",
        )
        .unwrap_err();
        assert!(e.contains("maps CPU 8 to more than one queue"), "{e}");
    }

    #[test]
    fn empty_mapping_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = []\n\n[turbine.xdp]\ntx = { eth0 = [0] \
             }\n",
        )
        .unwrap_err();
        assert!(e.contains("empty queue_to_cpu_mapping"), "{e}");
    }

    #[test]
    fn empty_tx_queue_list_is_error() {
        let e = resolve_with_user(
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 \
             }]\n\n[turbine.xdp]\ntx = { eth0 = [] }\n",
        )
        .unwrap_err();
        assert!(e.contains("lists no queues"), "{e}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        for input in [
            "[nonsense]\nfoo = 1\n",
            "[turbine]\nbogus = 1\n",
            "[turbine.xdp]\nbogus = 1\n",
            "[interfaces.\"eth0\"]\nbogus = 1\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8 }]\n",
            "[interfaces.\"eth0\"]\nqueue_to_cpu_mapping = [{ queue = 0, cpu = 8, bogus = 1 }]\n",
        ] {
            assert!(parse_str(input).is_err(), "must be rejected: {input}");
        }
    }

    #[test]
    fn unreadable_config_file_is_an_error() {
        let e = resolve_xdp_config(Some(Path::new("/nonexistent/agave/config.toml"))).unwrap_err();
        assert!(e.contains("failed to read config file"), "{e}");
    }
}
