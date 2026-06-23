//! Parser for the validator's `--config` TOML file.
//!
//! The file carries an optional `[xdp]` section (AF_XDP transmit) and any number
//! of `[threads.<name>]` sections, each pinning a managed thread to a CPU. The
//! server looks up the threads it knows by name (e.g. `poh`); other names parse
//! and are simply not consumed. Unknown *fields* are rejected so that unsupported
//! settings cannot be set silently.
//!
//! ```toml
//! version = 1
//!
//! [xdp]
//! interface = "ens1f0"                       # optional; omit to auto-detect
//! zero_copy = true                           # optional; default false
//! queue_to_cpu_mapping = ["0:8", "1:9"]      # required (if [xdp] present), non-empty
//!
//! [threads.poh]
//! cpus = [2]                                 # logical CPU(s): an index, a "0-3,8"
//!                                            # range string, or a list mixing both
//!                                            # (e.g. 2, "2-5", or ["2-5", 8])
//! ```
//!
//! Each `"<queue>:<cpu>"` entry binds one NIC queue to one CPU core, producing a
//! [`QueueCpuBinding`]. The mapping is one-to-one: no queue and no CPU may appear
//! more than once.

use {
    agave_xdp::transmitter::{QueueCpuBinding, XdpConfig},
    serde::Deserialize,
    solana_clap_utils::input_parsers::parse_cpu_ranges,
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

    /// `version` was not `1`.
    #[error("unsupported version {found} (expected 1)")]
    Version { found: u64 },

    /// `queue_to_cpu_mapping` resolved to no entries.
    #[error("queue_to_cpu_mapping must not be empty")]
    EmptyMapping,

    /// A mapping entry was not a single `"<queue>:<cpu>"` pair.
    #[error("invalid queue->cpu mapping `{input}` (expected \"<queue>:<cpu>\")")]
    MappingSyntax { input: String },

    /// A NIC queue was bound more than once across the mapping.
    #[error("queue {queue} is mapped more than once")]
    DuplicateQueue { queue: u32 },

    /// A CPU core was bound to more than one queue (the mapping must be
    /// one-to-one).
    #[error("CPU core {cpu} is mapped to more than one queue")]
    DuplicateCpu { cpu: usize },

    /// A thread that only supports single-CPU affinity was given a different
    /// number of CPUs.
    #[error("thread requires exactly one cpu, but {count} were specified")]
    SingleCpuRequired { count: usize },

    /// A `cpus` entry was not a valid `"0-3,8"` set string.
    #[error("invalid cpu set `{input}` (expected an index or a \"0-3,8\" range)")]
    CpuSet { input: String },
}

/// A parsed validator config file.
#[derive(Debug, Default)]
pub struct FileConfig {
    /// AF_XDP transmit configuration (`[xdp]`), present only if declared.
    pub xdp: Option<XdpConfig>,
    /// Managed threads (`[threads.<name>]`), keyed by name. More thread kinds
    /// are expected over time; callers look up the ones they care about by name.
    threads: BTreeMap<String, ThreadConfig>,
}

impl FileConfig {
    /// Configuration for the thread declared under `[threads.<name>]`, if any.
    /// E.g. `config.thread("poh")`.
    pub fn thread(&self, name: &str) -> Option<&ThreadConfig> {
        self.threads.get(name)
    }
}

/// Configuration of a single managed thread (`[threads.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadConfig {
    /// Logical CPU(s) the thread may run on, sorted and de-duplicated.
    pub cpus: Vec<usize>,
}

/// Proof of History thread configuration. PoH only supports single-CPU affinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PohConfig {
    /// Logical CPU the PoH thread is pinned to.
    pub cpu: usize,
}

impl TryFrom<&ThreadConfig> for PohConfig {
    type Error = ConfigFileError;

    /// PoH pins to a single CPU, so the thread must specify exactly one.
    fn try_from(thread: &ThreadConfig) -> Result<Self, Self::Error> {
        match thread.cpus.as_slice() {
            [cpu] => Ok(PohConfig { cpu: *cpu }),
            cpus => Err(ConfigFileError::SingleCpuRequired { count: cpus.len() }),
        }
    }
}

/// Serde DTO mirroring the literal TOML shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: u64,
    #[serde(default)]
    xdp: Option<RawXdp>,
    /// `[threads.<name>]` entries, keyed by name. Any name is accepted; the
    /// server only consumes the ones it knows.
    #[serde(default)]
    threads: BTreeMap<String, RawThread>,
}

/// A single `[threads.<name>]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawThread {
    cpus: RawCpus,
}

/// The `cpus` value: a single [`CpuSpec`] (e.g. `2` or `"2-5"`) or a list of them
/// (e.g. `[2, 3]` or `["2-5", 3]`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCpus {
    One(CpuSpec),
    Many(Vec<CpuSpec>),
}

/// A single CPU index (`2`) or a `"0-3,8"` set string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CpuSpec {
    Index(usize),
    Set(String),
}

/// Flatten the raw `cpus` value into a sorted, de-duplicated list of logical CPUs.
fn build_thread(raw: &RawThread) -> Result<ThreadConfig, ConfigFileError> {
    let specs = match &raw.cpus {
        RawCpus::One(spec) => std::slice::from_ref(spec),
        RawCpus::Many(specs) => specs.as_slice(),
    };
    let mut cpus = Vec::new();
    for spec in specs {
        match spec {
            CpuSpec::Index(cpu) => cpus.push(*cpu),
            CpuSpec::Set(set) => {
                let parsed = parse_cpu_ranges(set).map_err(|_| ConfigFileError::CpuSet {
                    input: set.clone(),
                })?;
                cpus.extend(parsed);
            }
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(ThreadConfig { cpus })
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

/// Parse the validator config file into a [`FileConfig`].
pub fn parse_config_file(path: &Path) -> Result<FileConfig, ConfigFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_str(&text)
}

fn parse_str(text: &str) -> Result<FileConfig, ConfigFileError> {
    let raw: RawFile = toml::from_str(text)?;
    if raw.version != 1 {
        return Err(ConfigFileError::Version {
            found: raw.version,
        });
    }

    let xdp = raw.xdp.map(build_xdp).transpose()?;
    let threads = raw
        .threads
        .iter()
        .map(|(name, raw)| Ok((name.clone(), build_thread(raw)?)))
        .collect::<Result<BTreeMap<_, _>, ConfigFileError>>()?;
    Ok(FileConfig { xdp, threads })
}

fn build_xdp(raw: RawXdp) -> Result<XdpConfig, ConfigFileError> {
    let entries = raw.queue_to_cpu_mapping.into_vec();
    if entries.is_empty() {
        return Err(ConfigFileError::EmptyMapping);
    }

    let mut queues = Vec::with_capacity(entries.len());
    let mut seen_queues = HashSet::new();
    let mut seen_cpus = HashSet::new();
    for entry in &entries {
        let (queue_str, cpu_str) =
            entry
                .split_once(':')
                .ok_or_else(|| ConfigFileError::MappingSyntax {
                    input: entry.clone(),
                })?;
        let queue: u32 = queue_str
            .trim()
            .parse()
            .map_err(|_| ConfigFileError::MappingSyntax {
                input: entry.clone(),
            })?;
        let cpu: usize = cpu_str
            .trim()
            .parse()
            .map_err(|_| ConfigFileError::MappingSyntax {
                input: entry.clone(),
            })?;
        if !seen_queues.insert(queue) {
            return Err(ConfigFileError::DuplicateQueue { queue });
        }
        if !seen_cpus.insert(cpu) {
            return Err(ConfigFileError::DuplicateCpu { cpu });
        }
        queues.push(QueueCpuBinding { queue, cpu });
    }

    Ok(XdpConfig::new(raw.interface, queues, raw.zero_copy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<FileConfig, ConfigFileError> {
        parse_str(s)
    }

    fn xdp(s: &str) -> XdpConfig {
        parse(s).unwrap().xdp.expect("[xdp] section")
    }

    #[test]
    fn minimal_auto_interface() {
        let c = xdp("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n");
        assert_eq!(c.interface, None);
        assert!(!c.zero_copy);
        assert_eq!(c.queues, vec![QueueCpuBinding { queue: 0, cpu: 8 }]);
    }

    #[test]
    fn full_fields() {
        let c = xdp(
            r#"
version = 1
[xdp]
interface = "ens1f0"
zero_copy = true
queue_to_cpu_mapping = ["0:2", "1:4", "2:7"]
"#,
        );
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
        let c = xdp("version = 1\n[xdp]\nqueue_to_cpu_mapping = [\"1:10\", \"3:11\", \"5:12\"]\n");
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
        let c = parse("version = 1\n[threads.poh]\ncpus = [3]\n").unwrap();
        assert!(c.xdp.is_none());
        assert_eq!(c.thread("poh"), Some(&ThreadConfig { cpus: vec![3] }));
        assert_eq!(c.thread("nonexistent"), None);
    }

    #[test]
    fn parses_multiple_threads() {
        // The parser keeps every declared thread; the server queries by name.
        let c =
            parse("version = 1\n[threads.poh]\ncpus = [2]\n[threads.replay]\ncpus = [8, 9]\n")
                .unwrap();
        assert_eq!(c.thread("poh"), Some(&ThreadConfig { cpus: vec![2] }));
        assert_eq!(c.thread("replay"), Some(&ThreadConfig { cpus: vec![8, 9] }));
    }

    #[test]
    fn parses_xdp_and_threads_together() {
        let c = parse(
            "version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n[threads.poh]\ncpus = [2]\n",
        )
        .unwrap();
        assert!(c.xdp.is_some());
        assert_eq!(c.thread("poh"), Some(&ThreadConfig { cpus: vec![2] }));
    }

    #[test]
    fn parses_cpu_forms() {
        // single index
        assert_eq!(
            parse("version = 1\n[threads.t]\ncpus = 2\n")
                .unwrap()
                .thread("t")
                .unwrap()
                .cpus,
            vec![2]
        );
        // single range string
        assert_eq!(
            parse("version = 1\n[threads.t]\ncpus = \"2-5\"\n")
                .unwrap()
                .thread("t")
                .unwrap()
                .cpus,
            vec![2, 3, 4, 5]
        );
        // list of indices
        assert_eq!(
            parse("version = 1\n[threads.t]\ncpus = [2, 3]\n")
                .unwrap()
                .thread("t")
                .unwrap()
                .cpus,
            vec![2, 3]
        );
        // mixed list, with overlap de-duplicated and sorted
        assert_eq!(
            parse("version = 1\n[threads.t]\ncpus = [\"2-5\", 3]\n")
                .unwrap()
                .thread("t")
                .unwrap()
                .cpus,
            vec![2, 3, 4, 5]
        );
    }

    #[test]
    fn rejects_bad_cpu_set() {
        let e = parse("version = 1\n[threads.t]\ncpus = \"2-x\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::CpuSet { .. }));
    }

    #[test]
    fn poh_requires_single_cpu() {
        let one = ThreadConfig { cpus: vec![2] };
        assert_eq!(PohConfig::try_from(&one).unwrap(), PohConfig { cpu: 2 });

        let many = ThreadConfig {
            cpus: vec![2, 3],
        };
        assert!(matches!(
            PohConfig::try_from(&many),
            Err(ConfigFileError::SingleCpuRequired { count: 2 })
        ));

        let none = ThreadConfig { cpus: vec![] };
        assert!(matches!(
            PohConfig::try_from(&none),
            Err(ConfigFileError::SingleCpuRequired { count: 0 })
        ));
    }

    #[test]
    fn empty_config_is_allowed() {
        // A file with no [xdp] and no threads is valid; it configures nothing.
        let c = parse("version = 1\n").unwrap();
        assert!(c.xdp.is_none());
        assert_eq!(c.thread("poh"), None);
    }

    #[test]
    fn rejects_wrong_version() {
        assert!(matches!(
            parse("version = 2\n[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n"),
            Err(ConfigFileError::Version { found: 2 })
        ));
    }

    #[test]
    fn rejects_unknown_field() {
        // `rx_size` is not a supported field, so it must be rejected.
        let e = parse("version = 1\n[xdp]\nrx_size = 8192\nqueue_to_cpu_mapping = \"0:8\"\n")
            .unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_thread_field() {
        // Any thread name is accepted, but unknown keys within a thread are not.
        let e = parse("version = 1\n[threads.poh]\ncpus = [1]\nbogus = 2\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_missing_mapping() {
        let e = parse("version = 1\n[xdp]\ninterface = \"eth0\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_duplicate_queue() {
        let e =
            parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = [\"0:8\", \"0:9\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateQueue { queue: 0 }));
    }

    #[test]
    fn rejects_duplicate_cpu() {
        let e =
            parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = [\"0:8\", \"1:8\"]\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::DuplicateCpu { cpu: 8 }));
    }

    #[test]
    fn rejects_bad_mapping_syntax() {
        let e = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0-1\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::MappingSyntax { .. }));
    }

    #[test]
    fn rejects_queue_range() {
        // Ranges/sets are no longer accepted; a single queue is required.
        let e = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0-3:8\"\n").unwrap_err();
        assert!(matches!(e, ConfigFileError::MappingSyntax { .. }));
    }
}
