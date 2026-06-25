//! Parser for the validator's `--config` TOML file.
//!
//! The server reads the sections it understands and rejects unknown *fields*
//! within them, so typos and unsupported settings fail loudly rather than being
//! silently ignored. Managed threads are declared under `[threads.<name>]` and
//! looked up by name, so new thread kinds need no parser change. The example
//! below is illustrative, not the full set of options.
//!
//! ```toml
//! [xdp]
//! interface = "ens1f0"                       # optional; omit to auto-detect
//! zero_copy = true                           # optional; default false
//! queue_to_cpu_mapping = ["0:8", "1:9"]      # required (if [xdp] present), non-empty
//!
//! [threads.poh]
//! cpu = 2                                    # logical CPU index (a single core)
//! reservation = "exclusive"                  # optional; "none" (default) or
//!                                            # "exclusive"
//! ```
//!
//! Each `"<queue>:<cpu>"` entry binds one NIC queue to one CPU core, producing a
//! [`QueueCpuBinding`]. The mapping is one-to-one: no queue and no CPU may appear
//! more than once.
//!
//! ## Reservations
//!
//! A `[threads.<name>]` may declare how it consumes its CPU via `reservation`:
//! `"none"` (the default) lets other work co-schedule on the same core, while
//! `"exclusive"` claims the core for that thread alone. XDP queue handler CPUs
//! (the cpus in `queue_to_cpu_mapping`) are *always* exclusive. Two exclusive
//! claimants may not overlap: mapping an XDP queue onto a CPU an exclusive thread
//! owns (or vice versa) is an error.

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

    /// Malformed TOML, a wrong value type, a missing required field, or an
    /// unknown field.
    #[error("malformed config: {0}")]
    Toml(#[from] toml::de::Error),

    /// `queue_to_cpu_mapping` was empty, or an entry was not a single
    /// `"<queue>:<cpu>"` pair.
    #[error("invalid queue_to_cpu_mapping: {0}")]
    Mapping(String),

    /// A NIC queue was bound more than once across the mapping.
    #[error("queue {queue} is mapped more than once")]
    DuplicateQueue { queue: u32 },

    /// A CPU core was bound to more than one queue (the mapping must be
    /// one-to-one).
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
    /// AF_XDP transmit configuration (`[xdp]`), present only if declared.
    pub xdp: Option<XdpConfig>,
    /// Managed threads (`[threads.<name>]`), keyed by name.
    threads: BTreeMap<String, ThreadConfig>,
}

impl ConfigFile {
    pub(crate) fn thread(&self, name: &str) -> Option<&ThreadConfig> {
        self.threads.get(name)
    }

    /// Insert or replace a `[threads.<name>]` block, used to fold in CLI overrides.
    pub(crate) fn set_thread(&mut self, name: impl Into<String>, thread: ThreadConfig) {
        self.threads.insert(name.into(), thread);
    }

    /// Names of all declared `[threads.<name>]` blocks.
    pub(crate) fn thread_names(&self) -> impl Iterator<Item = &str> {
        self.threads.keys().map(String::as_str)
    }

    /// The exclusively-claimed CPUs from `[threads.*]` (reservation = exclusive),
    /// keyed by CPU with the claiming thread's name. Errs if two threads claim the
    /// same CPU. Callers fold in XDP queue CPUs, which are exclusive too.
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

/// Record `owner`'s exclusive claim on `cpu` in `owners`, erroring if another
/// entity already claimed it. Conflicts are caught regardless of insertion order.
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

/// How a managed thread consumes the CPU capacity it is placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reservation {
    /// The entity shares its CPU(s); other work may co-schedule there.
    #[default]
    None,
    /// The entity owns its CPU(s) outright; no other exclusive claimant may use
    /// them. XDP queue handler CPUs are always exclusive in this sense.
    Exclusive,
}

/// Configuration of a single managed thread (`[threads.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadConfig {
    /// Logical CPU the thread pins to.
    pub cpu: usize,
    /// How the thread consumes its CPU. Defaults to [`Reservation::None`].
    pub reservation: Reservation,
}

/// Serde DTO mirroring the literal TOML shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    xdp: Option<RawXdp>,
    #[serde(default)]
    threads: BTreeMap<String, RawThread>,
}

/// A single `[threads.<name>]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawThread {
    cpu: usize,
    #[serde(default)]
    reservation: Option<RawReservation>,
}

/// The `reservation` value as written in the file.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawReservation {
    None,
    Exclusive,
}

fn build_thread(raw: &RawThread) -> ThreadConfig {
    let reservation = match raw.reservation {
        None | Some(RawReservation::None) => Reservation::None,
        Some(RawReservation::Exclusive) => Reservation::Exclusive,
    };
    ThreadConfig {
        cpu: raw.cpu,
        reservation,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawXdp {
    #[serde(default)]
    interface: Option<String>,
    #[serde(default)]
    zero_copy: bool,
    queue_to_cpu_mapping: StrOrVec,
}

/// A single `"queue:cpu"` mapping string, or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StrOrVec {
    One(String),
    Many(Vec<String>),
}

impl StrOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            StrOrVec::One(s) => vec![s],
            StrOrVec::Many(v) => v,
        }
    }
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
    let raw: RawFile = toml::from_str(text)?;
    let xdp = raw.xdp.map(xdp_config_from_raw).transpose()?;
    let threads = raw
        .threads
        .iter()
        .map(|(name, raw)| (name.clone(), build_thread(raw)))
        .collect::<BTreeMap<_, _>>();
    Ok(ConfigFile { xdp, threads })
}

fn xdp_config_from_raw(raw: RawXdp) -> Result<XdpConfig, ConfigFileError> {
    let entries = raw.queue_to_cpu_mapping.into_vec();
    if entries.is_empty() {
        return Err(ConfigFileError::Mapping("must not be empty".to_string()));
    }

    let mut queues = Vec::with_capacity(entries.len());
    let mut seen_queues = HashSet::new();
    let mut seen_cpus = HashSet::new();
    for entry in &entries {
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

    // An empty interface string means "unset"; auto-detect rather than try to bind
    // an interface named "".
    let interface = raw.interface.filter(|name| !name.is_empty());
    Ok(XdpConfig::new(interface, queues, raw.zero_copy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<ConfigFile, ConfigFileError> {
        parse_str(s)
    }

    fn xdp(s: &str) -> XdpConfig {
        parse(s).unwrap().xdp.expect("[xdp] section")
    }

    #[test]
    fn minimal_auto_interface() {
        let c = xdp("[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n");
        assert_eq!(c.interface, None);
        assert!(!c.zero_copy);
        assert_eq!(c.queues, vec![QueueCpuBinding { queue: 0, cpu: 8 }]);
    }

    #[test]
    fn empty_interface_is_auto_detect() {
        // An empty string is treated as unset, not a literal interface named "".
        let c = xdp("[xdp]\ninterface = \"\"\nqueue_to_cpu_mapping = \"0:8\"\n");
        assert_eq!(c.interface, None);
    }

    #[test]
    fn full_fields() {
        let c = xdp(r#"
[xdp]
interface = "ens1f0"
zero_copy = true
queue_to_cpu_mapping = ["0:2", "1:4", "2:7"]
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
    fn non_contiguous_queues() {
        let c = xdp("[xdp]\nqueue_to_cpu_mapping = [\"1:10\", \"3:11\", \"5:12\"]\n");
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
        assert!(c.xdp.is_none());
        assert_eq!(
            c.thread("poh"),
            Some(&ThreadConfig {
                cpu: 3,
                reservation: Reservation::None,
            })
        );
        assert_eq!(c.thread("nonexistent"), None);
    }

    #[test]
    fn parses_multiple_threads() {
        let c = parse("[threads.poh]\ncpu = 2\n[threads.replay]\ncpu = 8\n").unwrap();
        assert_eq!(
            c.thread("poh"),
            Some(&ThreadConfig {
                cpu: 2,
                reservation: Reservation::None,
            })
        );
        assert_eq!(
            c.thread("replay"),
            Some(&ThreadConfig {
                cpu: 8,
                reservation: Reservation::None,
            })
        );
    }

    #[test]
    fn parses_xdp_and_threads_together() {
        let c = parse("[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n[threads.poh]\ncpu = 2\n").unwrap();
        assert!(c.xdp.is_some());
        assert_eq!(
            c.thread("poh"),
            Some(&ThreadConfig {
                cpu: 2,
                reservation: Reservation::None,
            })
        );
    }

    #[test]
    fn parses_single_cpu() {
        assert_eq!(
            parse("[threads.t]\ncpu = 2\n")
                .unwrap()
                .thread("t")
                .unwrap()
                .cpu,
            2
        );
    }

    #[test]
    fn rejects_multiple_cpus() {
        // `cpu` accepts a single index only; a list (or range string) is rejected.
        let e = parse("[threads.t]\ncpu = [2, 3]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn reservation_defaults_to_none() {
        let t = parse("[threads.poh]\ncpu = 2\n")
            .unwrap()
            .thread("poh")
            .unwrap()
            .reservation;
        assert_eq!(t, Reservation::None);
    }

    #[test]
    fn parses_exclusive_reservation() {
        let t = parse("[threads.poh]\ncpu = 2\nreservation = \"exclusive\"\n")
            .unwrap()
            .thread("poh")
            .unwrap()
            .reservation;
        assert_eq!(t, Reservation::Exclusive);
    }

    #[test]
    fn rejects_unknown_reservation() {
        // An unrecognized reservation value is a hard (serde) error.
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
        // A `none` thread claims nothing, so it never conflicts.
        let c = parse("[threads.a]\ncpu = 5\nreservation = \"exclusive\"\n[threads.b]\ncpu = 5\n")
            .unwrap();
        assert!(c.exclusive_thread_cpus().unwrap().contains_key(&5));
    }

    #[test]
    fn empty_config_is_allowed() {
        let c = parse("").unwrap();
        assert!(c.xdp.is_none());
        assert_eq!(c.thread("poh"), None);
    }

    #[test]
    fn rejects_unknown_field() {
        // `rx_size` is not a supported field, so it must be rejected.
        let e = parse("[xdp]\nrx_size = 8192\nqueue_to_cpu_mapping = \"0:8\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_thread_field() {
        // Any thread name is accepted, but unknown keys within a thread are not.
        let e = parse("[threads.poh]\ncpu = 1\nbogus = 2\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_missing_mapping() {
        let e = parse("[xdp]\ninterface = \"eth0\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_duplicate_queue() {
        let e = parse("[xdp]\nqueue_to_cpu_mapping = [\"0:8\", \"0:9\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateQueue { queue: 0 }));
    }

    #[test]
    fn rejects_duplicate_cpu() {
        let e = parse("[xdp]\nqueue_to_cpu_mapping = [\"0:8\", \"1:8\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateCpu { cpu: 8 }));
    }

    #[test]
    fn rejects_bad_mapping_syntax() {
        let e = parse("[xdp]\nqueue_to_cpu_mapping = \"0-1\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }

    #[test]
    fn rejects_queue_range() {
        // Ranges/sets are no longer accepted; a single queue is required.
        let e = parse("[xdp]\nqueue_to_cpu_mapping = \"0-3:8\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }

    #[test]
    fn rejects_empty_mapping() {
        let e = parse("[xdp]\nqueue_to_cpu_mapping = []\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Mapping(_)));
    }
}
