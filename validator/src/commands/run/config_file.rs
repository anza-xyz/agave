//! Parser for the validator's `--config` TOML file.
//!
//! Unknown fields are rejected so typos fail loudly. Managed threads use
//! `[threads.<name>]`; XDP endpoints use `[xdp.<name>]`.
//!
//! ```toml
//! [interfaces."ens1f0"]
//! zero_copy = true                       # default false
//! queue_to_cpu_mapping = ["0:8", "1:9"]  # required, non-empty
//!
//! [xdp.tpu]
//! interfaces = ["ens1f0"] # omit to auto-detect
//!
//! [threads.poh]
//! cpu = 2
//! reservation = "exclusive" # "none" by default
//! ```
//!
//! A hardware queue is serviced by a single CPU-pinned worker thread, which is
//! a property of the NIC, not of any one endpoint — so `queue_to_cpu_mapping`
//! (and `zero_copy`) live on `[interfaces.<name>]`. An `[xdp.<name>]` endpoint
//! just names the interface(s) it reads from; endpoints sharing an interface
//! share its whole queue/CPU worker pool. Missing interface sections (or an
//! endpoint naming none) fall back to an auto-selected interface and CPU. The
//! validator currently accepts at most one XDP endpoint and one interface per
//! endpoint.
//!
//! Each `"<queue>:<cpu>"` mapping is one-to-one. XDP queue CPUs are always
//! exclusive; `[threads.<name>]` may opt in with `reservation = "exclusive"`.

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

    /// Multiple interfaces named for one XDP endpoint.
    #[error(
        "[xdp.{name}] references multiple interfaces ({interfaces}); only one interface per XDP \
         endpoint is currently supported"
    )]
    MultiInterfaceUnsupported { name: String, interfaces: String },

    /// A NIC queue was bound more than once across the mapping.
    #[error("queue {queue} is mapped more than once")]
    DuplicateQueue { queue: u32 },

    /// A CPU core was bound to more than one queue.
    #[error("CPU core {cpu} is mapped to more than one queue")]
    DuplicateCpu { cpu: usize },

    /// Two exclusive reservations requested the same CPU.
    #[error(
        "CPU {cpu} is exclusively reserved by both {first} and {second}; exclusive reservations \
         may not share a CPU core"
    )]
    ExclusiveCpuConflict {
        cpu: usize,
        first: String,
        second: String,
    },
}

/// A parsed validator config file.
#[derive(Debug, Default)]
pub struct ConfigFile {
    /// AF_XDP transmit endpoints, keyed by `[xdp.<name>]`.
    pub xdp: BTreeMap<String, XdpConfig>,
    /// Managed threads (`[threads.<name>]`), keyed by name.
    pub threads: BTreeMap<String, ThreadConfig>,
}

impl ConfigFile {
    /// Exclusive CPU claims from `[threads.*]`, keyed by CPU.
    pub(crate) fn exclusive_thread_cpus(&self) -> Result<BTreeMap<usize, String>, ConfigFileError> {
        let mut owners = BTreeMap::new();
        for (name, thread) in &self.threads {
            if thread.reservation == Reservation::Exclusive {
                claim_exclusive(&mut owners, thread.cpu, format!("[threads.{name}]"))?;
            }
        }
        Ok(owners)
    }
}

/// Add an exclusive CPU claim, erroring on conflicts.
pub(crate) fn claim_exclusive(
    owners: &mut BTreeMap<usize, String>,
    cpu: usize,
    owner: String,
) -> Result<(), ConfigFileError> {
    if let Some(first) = owners.get(&cpu) {
        return Err(ConfigFileError::ExclusiveCpuConflict {
            cpu,
            first: first.clone(),
            second: owner,
        });
    }
    owners.insert(cpu, owner);
    Ok(())
}

/// CPU-sharing policy for a managed thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reservation {
    /// Share this CPU with other work.
    #[default]
    None,
    /// Reserve this CPU from other exclusive users.
    Exclusive,
}

/// Configuration of a single managed thread (`[threads.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadConfig {
    /// Logical CPU the thread pins to.
    pub cpu: usize,
    /// CPU-sharing policy; defaults to [`Reservation::None`].
    #[serde(default)]
    pub reservation: Reservation,
}

/// TOML shape used for serde.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    interfaces: BTreeMap<String, RawInterface>,
    #[serde(default)]
    xdp: BTreeMap<String, RawXdpEndpoint>,
    #[serde(default)]
    threads: BTreeMap<String, ThreadConfig>,
}

/// Settings for one `[interfaces.<name>]`: the NIC's hardware queue -> CPU
/// worker assignment, shared by every endpoint that names this interface.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInterface {
    #[serde(default)]
    zero_copy: bool,
    queue_to_cpu_mapping: Vec<String>,
}

/// An `[interfaces.<name>]` entry once its `queue_to_cpu_mapping` is parsed
/// and validated.
struct Interface {
    zero_copy: bool,
    queues: Vec<QueueCpuBinding>,
}

/// A single `[xdp.<name>]` endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawXdpEndpoint {
    /// Interface names; empty means auto-detect.
    #[serde(default)]
    interfaces: Vec<String>,
}

/// Parse the validator config file into a [`ConfigFile`].
pub(crate) fn parse_config_file(path: &Path) -> Result<ConfigFile, ConfigFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_str(&text)
}

fn parse_str(text: &str) -> Result<ConfigFile, ConfigFileError> {
    let RawFile {
        interfaces,
        xdp,
        threads,
    } = toml::from_str(text)?;
    let interfaces = interfaces
        .into_iter()
        .map(|(name, raw)| interface_from_raw(raw).map(|iface| (name, iface)))
        .collect::<Result<BTreeMap<_, _>, ConfigFileError>>()?;
    let xdp = xdp
        .into_iter()
        .map(|(name, endpoint)| {
            xdp_config_from_raw(&name, endpoint, &interfaces).map(|config| (name, config))
        })
        .collect::<Result<BTreeMap<_, _>, ConfigFileError>>()?;
    Ok(ConfigFile { xdp, threads })
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
    name: &str,
    raw: RawXdpEndpoint,
    declared_interfaces: &BTreeMap<String, Interface>,
) -> Result<XdpConfig, ConfigFileError> {
    if raw.interfaces.len() > 1 {
        return Err(ConfigFileError::MultiInterfaceUnsupported {
            name: name.to_string(),
            interfaces: raw.interfaces.join(", "),
        });
    }
    let interface = raw.interfaces.into_iter().next();
    let (queues, zero_copy) = match interface.as_deref().and_then(|n| declared_interfaces.get(n))
    {
        Some(iface) => (iface.queues.clone(), iface.zero_copy),
        None => (Vec::new(), false),
    };

    Ok(XdpConfig::new(interface, queues, zero_copy))
}

#[cfg(test)]
mod tests {
    use super::{parse_str as parse, *};

    fn xdp(s: &str) -> XdpConfig {
        parse(s)
            .unwrap()
            .xdp
            .remove("tpu")
            .expect("[xdp.tpu] section")
    }

    #[test]
    fn minimal_endpoint_has_no_queues() {
        // No interfaces named, so there's nothing to resolve a queue mapping
        // from; the caller falls back to auto-selecting an interface and CPU.
        let c = xdp("[xdp.tpu]\n");
        assert_eq!(c.interface, None);
        assert!(!c.zero_copy);
        assert!(c.queues.is_empty());
    }

    #[test]
    fn interface_without_section_has_no_queues() {
        // The endpoint names an interface, but nothing declares its settings.
        let c = xdp("[xdp.tpu]\ninterfaces = [\"ens1f0\"]\n");
        assert_eq!(c.interface.as_deref(), Some("ens1f0"));
        assert!(!c.zero_copy);
        assert!(c.queues.is_empty());
    }

    #[test]
    fn unreferenced_interface_section_is_ignored() {
        // A declared interface only applies to endpoints that name it.
        let c = xdp(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\n\n[xdp.tpu]\n",
        );
        assert_eq!(c.interface, None);
        assert!(!c.zero_copy);
        assert!(c.queues.is_empty());
    }

    #[test]
    fn full_fields() {
        let c = xdp(r#"
[interfaces."ens1f0"]
zero_copy = true
queue_to_cpu_mapping = ["0:2", "1:4", "2:7"]

[xdp.tpu]
interfaces = ["ens1f0"]
"#);
        assert_eq!(c.interface.as_deref(), Some("ens1f0"));
        assert!(c.zero_copy);
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 0, cpu: 2 },
                QueueCpuBinding { queue: 1, cpu: 4 },
                QueueCpuBinding { queue: 2, cpu: 7 },
            ]
        );
    }

    #[test]
    fn endpoints_sharing_an_interface_get_its_whole_queue_pool() {
        let c = parse(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\", \"1:9\"]\n\n[xdp.tpu]\n\
             interfaces = [\"ens1f0\"]\n\n[xdp.gossip]\ninterfaces = [\"ens1f0\"]\n",
        )
        .unwrap();
        let expected = vec![
            QueueCpuBinding { queue: 0, cpu: 8 },
            QueueCpuBinding { queue: 1, cpu: 9 },
        ];
        assert_eq!(c.xdp["tpu"].queues, expected);
        assert_eq!(c.xdp["gossip"].queues, expected);
    }

    #[test]
    fn rejects_multiple_interfaces_per_endpoint() {
        let e = parse("[xdp.tpu]\ninterfaces = [\"ens1f0\", \"ens1f1\"]\n").unwrap_err();
        assert!(matches!(
            e,
            ConfigFileError::MultiInterfaceUnsupported { name, .. } if name == "tpu"
        ));
    }

    #[test]
    fn multiple_xdp_endpoints_parse() {
        // Parser policy allows multiple endpoints; execute.rs limits active use.
        let c = parse("[xdp.tpu]\n\n[xdp.gossip]\n").unwrap();
        assert_eq!(c.xdp.len(), 2);
        assert!(c.xdp.contains_key("tpu"));
        assert!(c.xdp.contains_key("gossip"));
    }

    #[test]
    fn non_contiguous_queues() {
        let c = xdp(
            "[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"1:10\", \"3:11\", \"5:12\"]\n\n\
             [xdp.tpu]\ninterfaces = [\"ens1f0\"]\n",
        );
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 1, cpu: 10 },
                QueueCpuBinding { queue: 3, cpu: 11 },
                QueueCpuBinding { queue: 5, cpu: 12 },
            ]
        );
    }

    #[test]
    fn queries_thread_by_name() {
        let c = parse("[threads.poh]\ncpu = 3\n").unwrap();
        assert!(c.xdp.is_empty());
        assert_eq!(
            c.threads.get("poh"),
            Some(&ThreadConfig {
                cpu: 3,
                reservation: Reservation::None,
            })
        );
        assert_eq!(c.threads.get("nonexistent"), None);
    }

    #[test]
    fn parses_multiple_threads() {
        let c = parse("[threads.poh]\ncpu = 2\n[threads.replay]\ncpu = 8\n").unwrap();
        assert_eq!(
            c.threads.get("poh"),
            Some(&ThreadConfig {
                cpu: 2,
                reservation: Reservation::None,
            })
        );
        assert_eq!(
            c.threads.get("replay"),
            Some(&ThreadConfig {
                cpu: 8,
                reservation: Reservation::None,
            })
        );
    }

    #[test]
    fn rejects_multiple_cpus() {
        // Only a single CPU index is valid.
        let e = parse("[threads.t]\ncpu = [2, 3]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn parses_exclusive_reservation() {
        let t = parse("[threads.poh]\ncpu = 2\nreservation = \"exclusive\"\n")
            .unwrap()
            .threads["poh"]
            .reservation;
        assert_eq!(t, Reservation::Exclusive);
    }

    #[test]
    fn rejects_unknown_reservation() {
        let e = parse("[threads.poh]\ncpu = 2\nreservation = \"bogus\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn exclusive_thread_cpus_collects_distinct() {
        let c = parse(
            "[threads.a]\ncpu = 2\nreservation = \"exclusive\"\n[threads.b]\ncpu = 3\nreservation \
             = \"exclusive\"\n",
        )
        .unwrap();
        let owners = c.exclusive_thread_cpus().unwrap();
        assert!(owners.contains_key(&2) && owners.contains_key(&3));
    }

    #[test]
    fn exclusive_thread_cpus_detects_conflict() {
        let c = parse(
            "[threads.a]\ncpu = 5\nreservation = \"exclusive\"\n[threads.b]\ncpu = 5\nreservation \
             = \"exclusive\"\n",
        )
        .unwrap();
        assert!(matches!(
            c.exclusive_thread_cpus(),
            Err(ConfigFileError::ExclusiveCpuConflict { cpu: 5, .. })
        ));
    }

    #[test]
    fn exclusive_thread_cpus_ignores_non_exclusive() {
        // Non-exclusive threads do not claim CPUs.
        let c = parse("[threads.a]\ncpu = 5\nreservation = \"exclusive\"\n[threads.b]\ncpu = 5\n")
            .unwrap();
        assert!(c.exclusive_thread_cpus().unwrap().contains_key(&5));
    }

    #[test]
    fn empty_config_is_allowed() {
        let c = parse("").unwrap();
        assert!(c.xdp.is_empty());
        assert!(c.threads.is_empty());
    }

    #[test]
    fn rejects_unknown_field() {
        let e = parse("[xdp.tpu]\nrx_size = 8192\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_interface_field() {
        // Names are open; fields are not.
        let e =
            parse("[interfaces.\"ens1f0\"]\nqueue_to_cpu_mapping = [\"0:8\"]\nbogus = 2\n")
                .unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_thread_field() {
        // Names are open; fields are not.
        let e = parse("[threads.poh]\ncpu = 1\nbogus = 2\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_missing_mapping() {
        // queue_to_cpu_mapping is required once an interface is declared.
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
