//! Parser for the `--xdp-config-file` TOML file.
//!
//! Configuration file for XDP. Unknown fields are rejected so that unsupported
//! settings cannot be set silently.
//!
//! ```toml
//! version = 1
//!
//! [xdp]
//! interface = "ens1f0"                       # optional; omit to auto-detect
//! zero_copy = true                           # optional; default false
//! queue_to_cpu_mapping = ["0-3:8", "4-7:9"]  # required, non-empty
//! ```
//!
//! Each `"<queues>:<cpu>"` entry binds every queue in the set (`0-3,8` grammar)
//! to `cpu`, producing one [`QueueCpuBinding`] per queue. Queues must be disjoint
//! across the whole mapping.

use {
    agave_xdp::transmitter::{QueueCpuBinding, XdpConfig},
    serde::Deserialize,
    std::{
        collections::HashSet,
        path::{Path, PathBuf},
    },
};

/// A failure to load or parse an XDP config file.
#[derive(Debug, thiserror::Error)]
pub enum XdpConfigFileError {
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

    /// A mapping entry was not in `"<queues>:<cpu>"` form.
    #[error("invalid queue->cpu mapping `{input}` (expected \"<queues>:<cpu>\")")]
    MappingSyntax { input: String },

    /// The queue side of an entry was not a valid `"0-3,8"` set.
    #[error("invalid queue set in mapping `{input}` (bad syntax, reversed range, or empty)")]
    BadQueueSet { input: String },

    /// A NIC queue was bound more than once across the mapping.
    #[error("queue {queue} is mapped more than once")]
    DuplicateQueue { queue: u32 },
}

/// Serde DTO mirroring the literal TOML shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: u64,
    xdp: RawXdp,
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

/// A single `"queues:cpu"` mapping string, or a list of them.
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

/// Parse an XDP config file into an [`XdpConfig`].
pub fn parse_xdp_config_file(path: &Path) -> Result<XdpConfig, XdpConfigFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| XdpConfigFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_str(&text)
}

fn parse_str(text: &str) -> Result<XdpConfig, XdpConfigFileError> {
    let raw: RawFile = toml::from_str(text)?;
    if raw.version != 1 {
        return Err(XdpConfigFileError::Version {
            found: raw.version,
        });
    }

    let entries = raw.xdp.queue_to_cpu_mapping.into_vec();
    if entries.is_empty() {
        return Err(XdpConfigFileError::EmptyMapping);
    }

    let mut queues = Vec::new();
    let mut seen = HashSet::new();
    for entry in &entries {
        let (queue_str, cpu_str) =
            entry
                .split_once(':')
                .ok_or_else(|| XdpConfigFileError::MappingSyntax {
                    input: entry.clone(),
                })?;
        let cpu: usize =
            cpu_str
                .trim()
                .parse()
                .map_err(|_| XdpConfigFileError::MappingSyntax {
                    input: entry.clone(),
                })?;
        let queue_ids =
            parse_queue_set(queue_str.trim()).ok_or_else(|| XdpConfigFileError::BadQueueSet {
                input: entry.clone(),
            })?;
        for queue in queue_ids {
            if !seen.insert(queue) {
                return Err(XdpConfigFileError::DuplicateQueue { queue });
            }
            queues.push(QueueCpuBinding { queue, cpu });
        }
    }

    Ok(XdpConfig::new(raw.xdp.interface, queues, raw.xdp.zero_copy))
}

/// Parse a `"0-3,8"`-style queue set into its ids. Returns `None` on bad syntax,
/// a reversed range, or an empty set.
fn parse_queue_set(input: &str) -> Option<Vec<u32>> {
    if input.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: u32 = lo.trim().parse().ok()?;
                let hi: u32 = hi.trim().parse().ok()?;
                if lo > hi {
                    return None;
                }
                out.extend(lo..=hi);
            }
            None => out.push(part.parse().ok()?),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<XdpConfig, XdpConfigFileError> {
        parse_str(s)
    }

    #[test]
    fn minimal_auto_interface() {
        let c = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n").unwrap();
        assert_eq!(c.interface, None);
        assert!(!c.zero_copy);
        assert_eq!(c.queues, vec![QueueCpuBinding { queue: 0, cpu: 8 }]);
    }

    #[test]
    fn full_fields() {
        let c = parse(
            r#"
version = 1
[xdp]
interface = "ens1f0"
zero_copy = true
queue_to_cpu_mapping = ["0:2", "1:4", "2:7"]
"#,
        )
        .unwrap();
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
    fn range_expands_to_one_binding_per_queue() {
        let c = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = [\"0-3:8\", \"4-5:9\"]\n").unwrap();
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 0, cpu: 8 },
                QueueCpuBinding { queue: 1, cpu: 8 },
                QueueCpuBinding { queue: 2, cpu: 8 },
                QueueCpuBinding { queue: 3, cpu: 8 },
                QueueCpuBinding { queue: 4, cpu: 9 },
                QueueCpuBinding { queue: 5, cpu: 9 },
            ]
        );
    }

    #[test]
    fn non_contiguous_queues() {
        let c = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"1,3,5:10\"\n").unwrap();
        assert_eq!(
            c.queues,
            vec![
                QueueCpuBinding { queue: 1, cpu: 10 },
                QueueCpuBinding { queue: 3, cpu: 10 },
                QueueCpuBinding { queue: 5, cpu: 10 },
            ]
        );
    }

    #[test]
    fn rejects_wrong_version() {
        assert!(matches!(
            parse("version = 2\n[xdp]\nqueue_to_cpu_mapping = \"0:8\"\n"),
            Err(XdpConfigFileError::Version { found: 2 })
        ));
    }

    #[test]
    fn rejects_unknown_field() {
        // `rx_size` is not a supported field, so it must be rejected.
        let e = parse("version = 1\n[xdp]\nrx_size = 8192\nqueue_to_cpu_mapping = \"0:8\"\n")
            .unwrap_err();
        assert!(matches!(e, XdpConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_missing_mapping() {
        let e = parse("version = 1\n[xdp]\ninterface = \"eth0\"\n").unwrap_err();
        assert!(matches!(e, XdpConfigFileError::Toml(_)));
    }

    #[test]
    fn rejects_duplicate_queue() {
        let e = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = [\"0-1:8\", \"1-2:9\"]\n")
            .unwrap_err();
        assert!(matches!(
            e,
            XdpConfigFileError::DuplicateQueue { queue: 1 }
        ));
    }

    #[test]
    fn rejects_bad_mapping_syntax() {
        let e = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"0-1\"\n").unwrap_err();
        assert!(matches!(e, XdpConfigFileError::MappingSyntax { .. }));
    }

    #[test]
    fn rejects_reversed_range() {
        let e = parse("version = 1\n[xdp]\nqueue_to_cpu_mapping = \"5-3:8\"\n").unwrap_err();
        assert!(matches!(e, XdpConfigFileError::BadQueueSet { .. }));
    }
}
