//! CPU topology discovery.

use {
    crate::CpuId,
    std::{
        fs, io,
        path::{Path, PathBuf},
    },
};

const CPU_SYSFS_PATH: &str = "/sys/devices/system/cpu";

/// Kernel cache type from sysfs `cache/indexN/type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheType {
    Data,
    Instruction,
    Unified,
}

/// Kernel cache ID scoped by Linux's cache level and type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheId {
    /// `level` from sysfs.
    pub level: usize,
    /// `type` from sysfs.
    pub cache_type: CacheType,
    /// `id` from sysfs.
    pub id: usize,
}

/// Kernel-reported topology for one logical CPU.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CpuTopology {
    /// Logical CPU ID.
    pub cpu_id: CpuId,
    /// `physical_package_id` from sysfs.
    pub physical_package_id: usize,
    /// `core_id` from sysfs.
    pub core_id: usize,
    /// `thread_siblings_list` from sysfs.
    pub thread_siblings_list: Vec<CpuId>,
    /// NUMA node parsed from the sysfs `nodeN` entry for this CPU.
    pub node_id: usize,
    /// L2 cache level, type, and ID from sysfs, if Linux exposes one for this CPU.
    pub l2_cache_id: Option<CacheId>,
    /// L3 cache level, type, and ID from sysfs, if Linux exposes one for this CPU.
    pub l3_cache_id: Option<CacheId>,
}

/// Get kernel-reported topology for the supplied logical CPUs.
///
/// Results are returned in the same order as the input CPUs.
///
/// # Arguments
/// * `cpus` - CPU IDs to query.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> std::io::Result<()> {
/// let allowed = cpu_affinity(None)?;
/// let topology = cpu_topology(allowed)?;
/// println!("CPU topology: {:?}", topology);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`io::Error`] if reading or parsing kernel sysfs topology fails.
pub fn cpu_topology(cpus: impl IntoIterator<Item = CpuId>) -> io::Result<Vec<CpuTopology>> {
    let cpus = cpus.into_iter();
    let mut topology = Vec::with_capacity(cpus.size_hint().0);

    for cpu_id in cpus {
        let (l2_cache_id, l3_cache_id) = read_cache_topology(cpu_id)?;
        topology.push(CpuTopology {
            cpu_id,
            physical_package_id: read_topology_id(cpu_id, "physical_package_id")?,
            core_id: read_topology_id(cpu_id, "core_id")?,
            thread_siblings_list: read_topology_cpu_list(cpu_id, "thread_siblings_list")?,
            node_id: read_node_id(cpu_id)?,
            l2_cache_id,
            l3_cache_id,
        });
    }

    Ok(topology)
}

/// Get all online logical CPUs from sysfs.
///
/// # Errors
///
/// Returns [`io::Error`] if reading or parsing the kernel sysfs CPU list fails.
pub fn online_cpus() -> io::Result<Vec<CpuId>> {
    read_cpu_list(format!("{CPU_SYSFS_PATH}/online"))
}

fn read_topology_id(cpu_id: CpuId, field: &str) -> io::Result<usize> {
    read_usize(format!("{CPU_SYSFS_PATH}/cpu{}/topology/{field}", *cpu_id))
}

fn read_topology_cpu_list(cpu_id: CpuId, field: &str) -> io::Result<Vec<CpuId>> {
    read_cpu_list(format!("{CPU_SYSFS_PATH}/cpu{}/topology/{field}", *cpu_id))
}

fn read_node_id(cpu_id: CpuId) -> io::Result<usize> {
    let cpu_dir = PathBuf::from(format!("{CPU_SYSFS_PATH}/cpu{}", *cpu_id));

    for entry in fs::read_dir(&cpu_dir)? {
        let path = entry?.path();
        let Some(node) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("node"))
        else {
            continue;
        };

        return parse_topology_id(node);
    }

    Err(io::Error::other("cpu sysfs node entry missing"))
}

fn read_cache_topology(cpu_id: CpuId) -> io::Result<(Option<CacheId>, Option<CacheId>)> {
    let cache_dir = PathBuf::from(format!("{CPU_SYSFS_PATH}/cpu{}/cache", *cpu_id));
    let mut l2_cache_id = None;
    let mut l3_cache_id = None;

    for entry in fs::read_dir(&cache_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("index") {
            continue;
        }

        let level = read_usize(path.join("level"))?;
        let cache_type = match read_trimmed(path.join("type"))?.as_str() {
            "Data" => CacheType::Data,
            "Instruction" => CacheType::Instruction,
            "Unified" => CacheType::Unified,
            _ => continue,
        };

        if cache_type == CacheType::Instruction || (level != 2 && level != 3) {
            continue;
        }

        let id = match read_usize(path.join("id")) {
            Ok(id) => id,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        match level {
            2 => {
                l2_cache_id = Some(CacheId {
                    level,
                    cache_type,
                    id,
                });
            }
            3 => {
                l3_cache_id = Some(CacheId {
                    level,
                    cache_type,
                    id,
                });
            }
            _ => {}
        }
    }

    Ok((l2_cache_id, l3_cache_id))
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    fs::read_to_string(path).map(|value| value.trim().to_string())
}

fn parse_topology_id(value: &str) -> io::Result<usize> {
    value
        .trim()
        .parse()
        .map_err(|_| io::Error::other("invalid sysfs topology id"))
}

fn read_usize(path: impl AsRef<Path>) -> io::Result<usize> {
    let value = read_trimmed(path)?;
    parse_topology_id(&value)
}

fn read_cpu_list(path: impl AsRef<Path>) -> io::Result<Vec<CpuId>> {
    let value = read_trimmed(path)?;
    parse_cpu_list(&value)
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<CpuId>> {
    let mut cpus = Vec::new();

    for part in value.trim().split(',') {
        match part.split_once('-') {
            Some((start, end)) => {
                let start = parse_topology_id(start)?;
                let end = parse_topology_id(end)?;
                if end < start {
                    return Err(io::Error::other("invalid sysfs CPU range"));
                }
                for cpu in start..=end {
                    cpus.push(
                        CpuId::new(cpu).map_err(|_| io::Error::other("invalid sysfs CPU id"))?,
                    );
                }
            }
            None => cpus.push(
                CpuId::new(parse_topology_id(part)?)
                    .map_err(|_| io::Error::other("invalid sysfs CPU id"))?,
            ),
        }
    }

    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_topology_id() {
        assert_eq!(parse_topology_id("42\n").unwrap(), 42);
    }

    #[test]
    fn test_parse_cpu_list() {
        assert_eq!(parse_cpu_list("0,2-4\n").unwrap(), cpu_ids([0, 2, 3, 4]));
        assert_eq!(
            parse_cpu_list("4-2").unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(
            parse_cpu_list(&libc::CPU_SETSIZE.to_string())
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn test_parse_topology_id_errors_are_other() {
        assert_eq!(
            parse_topology_id("not-a-number").unwrap_err().kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn test_cpu_topology_helpers() {
        let cpu0 = CpuTopology {
            cpu_id: CpuId::new(0).unwrap(),
            physical_package_id: 0,
            core_id: 0,
            thread_siblings_list: cpu_ids([0, 8]),
            node_id: 0,
            l2_cache_id: Some(CacheId {
                level: 2,
                cache_type: CacheType::Unified,
                id: 0,
            }),
            l3_cache_id: Some(CacheId {
                level: 3,
                cache_type: CacheType::Unified,
                id: 0,
            }),
        };
        let cpu1 = CpuTopology {
            cpu_id: CpuId::new(1).unwrap(),
            physical_package_id: 0,
            core_id: 1,
            thread_siblings_list: cpu_ids([1, 9]),
            node_id: 0,
            l2_cache_id: Some(CacheId {
                level: 2,
                cache_type: CacheType::Unified,
                id: 1,
            }),
            l3_cache_id: Some(CacheId {
                level: 3,
                cache_type: CacheType::Unified,
                id: 0,
            }),
        };

        assert!(cpu0.thread_siblings_list.contains(&CpuId::new(8).unwrap()));
        assert!(!cpu0.thread_siblings_list.contains(&cpu1.cpu_id));
        assert_ne!(cpu0.l2_cache_id, cpu1.l2_cache_id);
        assert_eq!(cpu0.l3_cache_id, cpu1.l3_cache_id);
    }

    #[test]
    fn test_cpu_topology_preserves_input_order() {
        let cpus = crate::cpu_affinity(None).expect("failed to query current CPU affinity");
        let cpus = cpus.into_iter().take(2).collect::<Vec<_>>();

        let topology = cpu_topology(cpus.iter().copied()).expect("failed to query CPU topology");

        assert_eq!(topology.len(), cpus.len());
        assert!(
            topology
                .iter()
                .map(|topology| topology.cpu_id)
                .eq(cpus.iter().copied())
        );
    }

    #[test]
    fn test_online_cpus_returns_sorted() {
        let cpus = online_cpus().expect("failed to query online CPUs");
        assert!(
            cpus.windows(2).all(|window| *window[0] <= *window[1]),
            "online_cpus should return sorted CPU list"
        );
    }

    fn cpu_ids(cpus: impl IntoIterator<Item = usize>) -> Vec<CpuId> {
        cpus.into_iter()
            .map(|cpu| CpuId::new(cpu).unwrap())
            .collect()
    }
}
