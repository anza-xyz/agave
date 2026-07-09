//! Validator `--config` TOML parser.
//!
//! Unknown fields are rejected so typos fail loudly.
//!
//! ```toml
//! [interfaces."ens1f0"]
//! zero_copy = true                       # default false
//! queue_to_cpu_mapping = ["0:8", "1:9"]  # required, non-empty
//!
//! [tpu.xdp]
//! tx = ["ens1f0:0", "ens1f0:1"]          # required, non-empty
//! ```
//!
//! Queue-to-CPU mappings are one-to-one. `[tpu.xdp]` currently supports a
//! single interface and TX only; if the section is present its `tx` must be
//! non-empty (auto-selection is the no-config default, not a file option).

use {
    agave_xdp::transmitter::{QueueCpuBinding, XdpConfig},
    serde::Deserialize,
    std::{
        collections::{BTreeMap, HashSet},
        path::{Path, PathBuf},
    },
};

/// A failure to load or parse the validator config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    /// The file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// TOML syntax, type, required-field, or unknown-field error.
    #[error("malformed config: {0}")]
    Toml(#[from] toml::de::Error),

    /// Empty or malformed `queue_to_cpu_mapping`.
    #[error("invalid queue_to_cpu_mapping: {0}")]
    Mapping(String),

    /// `[tpu.xdp]` named more than one interface across its `tx` entries.
    #[error(
        "[tpu.xdp] references multiple interfaces ({interfaces}); only one interface per XDP \
         endpoint is currently supported"
    )]
    MultiInterfaceUnsupported { interfaces: String },

    /// `[tpu.xdp]` was declared with an empty `tx`.
    #[error("[tpu.xdp] requires a non-empty `tx` (omit the [tpu.xdp] section to auto-select)")]
    EmptyTx,

    /// A malformed `tx` queue selector entry.
    #[error("invalid queue selector: {0}")]
    Selector(String),

    /// A `tx` entry named a queue absent from the interface's
    /// `queue_to_cpu_mapping`.
    #[error(
        "[tpu.xdp] references queue {queue} on interface \"{interface}\", but it is not in \
         [interfaces.\"{interface}\"] queue_to_cpu_mapping"
    )]
    UnknownQueue { interface: String, queue: u32 },

    /// A `tx` selector was listed more than once.
    #[error("[tpu.xdp] references queue {queue} on interface \"{interface}\" more than once")]
    DuplicateTxSelector { interface: String, queue: u32 },

    /// A NIC queue was bound more than once across the mapping.
    #[error("queue {queue} is mapped more than once")]
    DuplicateQueue { queue: u32 },

    /// A CPU core was bound to more than one queue.
    #[error("CPU core {cpu} is mapped to more than one queue")]
    DuplicateCpu { cpu: usize },
}

/// A parsed validator config file.
#[derive(Debug, Default)]
pub struct ConfigFile {
    /// TPU networking configuration (`[tpu]`).
    pub tpu: TpuConfig,
}

/// TPU networking configuration (`[tpu]`).
#[derive(Debug, Default)]
pub struct TpuConfig {
    /// AF_XDP transmit endpoint (`[tpu.xdp]`), if declared.
    pub xdp: Option<XdpConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    interfaces: BTreeMap<String, RawInterface>,
    #[serde(default)]
    tpu: RawTpu,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTpu {
    #[serde(default)]
    xdp: Option<RawXdpEndpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInterface {
    #[serde(default)]
    zero_copy: bool,
    queue_to_cpu_mapping: Vec<String>,
}

struct Interface {
    zero_copy: bool,
    queues: Vec<QueueCpuBinding>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawXdpEndpoint {
    #[serde(default)]
    tx: Vec<String>,
}

pub(crate) fn parse_config_file(path: &Path) -> Result<ConfigFile, ConfigFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_str(&text)
}

fn parse_str(text: &str) -> Result<ConfigFile, ConfigFileError> {
    let RawFile { interfaces, tpu } = toml::from_str(text)?;
    let interfaces = interfaces
        .into_iter()
        .map(|(name, raw)| interface_from_raw(raw).map(|iface| (name, iface)))
        .collect::<Result<BTreeMap<_, _>, ConfigFileError>>()?;
    let xdp = tpu
        .xdp
        .map(|endpoint| xdp_config_from_raw(endpoint, &interfaces))
        .transpose()?;
    Ok(ConfigFile {
        tpu: TpuConfig { xdp },
    })
}

fn interface_from_raw(raw: RawInterface) -> Result<Interface, ConfigFileError> {
    if raw.queue_to_cpu_mapping.is_empty() {
        return Err(ConfigFileError::Mapping("must not be empty".to_string()));
    }

    let mut queues = Vec::with_capacity(raw.queue_to_cpu_mapping.len());
    let mut seen_queues = HashSet::new();
    let mut seen_cpus = HashSet::new();
    for entry in &raw.queue_to_cpu_mapping {
        let mapping_err =
            || ConfigFileError::Mapping(format!("`{entry}` is not a \"<queue>:<cpu>\" pair"));
        let (queue_str, cpu_str) = entry.split_once(':').ok_or_else(mapping_err)?;
        let queue: u32 = queue_str.trim().parse().map_err(|_| mapping_err())?;
        let cpu: usize = cpu_str.trim().parse().map_err(|_| mapping_err())?;
        if !seen_queues.insert(queue) {
            return Err(ConfigFileError::DuplicateQueue { queue });
        }
        if !seen_cpus.insert(cpu) {
            return Err(ConfigFileError::DuplicateCpu { cpu });
        }
        queues.push(QueueCpuBinding { queue, cpu });
    }

    Ok(Interface {
        zero_copy: raw.zero_copy,
        queues,
    })
}

fn xdp_config_from_raw(
    raw: RawXdpEndpoint,
    declared_interfaces: &BTreeMap<String, Interface>,
) -> Result<XdpConfig, ConfigFileError> {
    let RawXdpEndpoint { tx } = raw;

    if tx.is_empty() {
        return Err(ConfigFileError::EmptyTx);
    }

    let mut interfaces = Vec::new();
    let mut queues = Vec::with_capacity(tx.len());
    let mut seen_selectors = HashSet::new();
    for entry in &tx {
        let (interface, queue) = parse_tx_selector(entry)?;
        if !seen_selectors.insert((interface.to_string(), queue)) {
            return Err(ConfigFileError::DuplicateTxSelector {
                interface: interface.to_string(),
                queue,
            });
        }
        queues.push(resolve_queue_binding(
            interface,
            queue,
            declared_interfaces,
        )?);
        if !interfaces.iter().any(|n| n == interface) {
            interfaces.push(interface.to_string());
        }
    }

    if interfaces.len() > 1 {
        return Err(ConfigFileError::MultiInterfaceUnsupported {
            interfaces: interfaces.join(", "),
        });
    }

    let interface = interfaces.into_iter().next();
    let zero_copy = interface
        .as_deref()
        .and_then(|n| declared_interfaces.get(n))
        .is_some_and(|iface| iface.zero_copy);

    Ok(XdpConfig::new(interface, queues, zero_copy))
}

fn parse_tx_selector(entry: &str) -> Result<(&str, u32), ConfigFileError> {
    let selector_err = || {
        ConfigFileError::Selector(format!(
            "`{entry}` in [tpu.xdp] tx must be a \"<interface>:<queue>\" pair"
        ))
    };
    let (interface, queue_str) = entry.split_once(':').ok_or_else(selector_err)?;
    let queue: u32 = queue_str.trim().parse().map_err(|_| selector_err())?;
    Ok((interface.trim(), queue))
}

fn resolve_queue_binding(
    interface: &str,
    queue: u32,
    declared_interfaces: &BTreeMap<String, Interface>,
) -> Result<QueueCpuBinding, ConfigFileError> {
    declared_interfaces
        .get(interface)
        .and_then(|iface| iface.queues.iter().find(|binding| binding.queue == queue))
        .copied()
        .ok_or_else(|| ConfigFileError::UnknownQueue {
            interface: interface.to_string(),
            queue,
        })
}

#[cfg(test)]
mod tests {
    use super::{parse_str as parse, *};

    fn xdp(s: &str) -> XdpConfig {
        parse(s).unwrap().tpu.xdp.expect("[tpu.xdp] section")
    }

    #[test]
    fn rejects_empty_tx() {
        // A present [tpu.xdp] must name its TX queues; auto-selection is only
        // the no-config default, not a file option.
        let e = parse("[tpu.xdp]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::EmptyTx));
        let e = parse("[tpu.xdp]\ntx = []\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::EmptyTx));
    }

    #[test]
    fn tx_selector_trims_whitespace() {
        let c = xdp(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n\n[tpu.xdp]\ntx = \
             [\"ens1f0 : 0\"]\n",
        );
        assert_eq!(c.interface.as_deref(), Some("ens1f0"));
        assert_eq!(c.queues, vec![QueueCpuBinding { queue: 0, cpu: 8 }]);
    }

    #[test]
    fn unreferenced_interface_section_is_ignored() {
        let c = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n").unwrap();
        assert!(c.tpu.xdp.is_none());
    }

    #[test]
    fn tx_pins_specific_queues() {
        let c = xdp(r#"
[interfaces."ens1f0"]
zero_copy = true
queue_to_cpu_mapping = ["0:2", "1:4", "2:7"]

[tpu.xdp]
tx = ["ens1f0:0", "ens1f0:2"]
"#);
        assert_eq!(c.interface.as_deref(), Some("ens1f0"));
        assert!(c.zero_copy);
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 0, cpu: 2 },
                QueueCpuBinding { queue: 2, cpu: 7 },
            ]
        );
    }

    #[test]
    fn tx_non_contiguous_queues() {
        let c = xdp(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"1:10\", \"3:11\", \
             \"5:12\"]\n\n[tpu.xdp]\ntx = [\"ens1f0:1\", \"ens1f0:5\"]\n",
        );
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 1, cpu: 10 },
                QueueCpuBinding { queue: 5, cpu: 12 },
            ]
        );
    }

    #[test]
    fn tx_rejects_bare_interface_name() {
        let e = parse(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n\n[tpu.xdp]\ntx = \
             [\"ens1f0\"]\n",
        )
        .unwrap_err();
        assert!(matches!(e, ConfigFileError::Selector(_)));
    }

    #[test]
    fn tx_rejects_unknown_queue() {
        let e = parse(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n\n[tpu.xdp]\ntx = \
             [\"ens1f0:5\"]\n",
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigFileError::UnknownQueue {
                interface,
                queue: 5,
            } if interface == "ens1f0"
        ));
    }

    #[test]
    fn tx_rejects_unknown_interface() {
        let e = parse("[tpu.xdp]\ntx = [\"ens1f0:0\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::UnknownQueue { .. }));
    }

    #[test]
    fn tx_rejects_multiple_interfaces() {
        let e = parse(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = \
             [\"0:8\"]\n\n[interfaces.\"ens1f1\"]\nqueue_to_cpu_mapping = \
             [\"0:9\"]\n\n[tpu.xdp]\ntx = [\"ens1f0:0\", \"ens1f1:0\"]\n",
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigFileError::MultiInterfaceUnsupported { .. }
        ));
    }

    #[test]
    fn tx_rejects_duplicate_selector() {
        let e = parse(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n\n[tpu.xdp]\ntx = \
             [\"ens1f0:0\", \"ens1f0:0\"]\n",
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigFileError::DuplicateTxSelector {
                interface,
                queue: 0,
            } if interface == "ens1f0"
        ));
    }

    #[test]
    fn rejects_rx_field() {
        let e = parse("[tpu.xdp]\nrx = [\"ens1f0:0\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_tpu_field() {
        let e = parse("[tpu]\nbogus = 1\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_threads_section() {
        let e = parse("[threads.poh]\ncpu = 3\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn empty_config_is_allowed() {
        let c = parse("").unwrap();
        assert!(c.tpu.xdp.is_none());
    }

    #[test]
    fn rejects_unknown_interface_field() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\nbogus = 2\n")
            .unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_missing_mapping() {
        let e = parse("[interfaces.\"ens1f0\"]\nzero_copy = true\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_duplicate_queue() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\", \"0:9\"]\n")
            .unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateQueue { queue: 0 }));
    }

    #[test]
    fn rejects_duplicate_cpu() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\", \"1:8\"]\n")
            .unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateCpu { cpu: 8 }));
    }

    #[test]
    fn rejects_bad_mapping_syntax() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0-1\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }

    #[test]
    fn rejects_queue_range() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0-3:8\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }

    #[test]
    fn rejects_empty_mapping() {
        let e = parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = []\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }
}
