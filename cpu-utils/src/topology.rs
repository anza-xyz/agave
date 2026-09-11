//! CPU topology discovery.

use {
    crate::{CpuId, affinity::CPU_SETSIZE},
    std::{
        collections::HashMap,
        fs, io,
        path::{Path, PathBuf},
    },
};

const CPU_SYSFS_PATH: &str = "/sys/devices/system/cpu";
const NODE_SYSFS_PATH: &str = "/sys/devices/system/node";

/// Kernel cache type from sysfs `cache/indexN/type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheType {
    Data,
    Instruction,
    Unified,
}

impl CacheType {
    /// Whether this cache can contain data.
    pub const fn supports_data(self) -> bool {
        matches!(self, Self::Data | Self::Unified)
    }

    /// Whether this cache can contain instructions.
    pub const fn supports_instructions(self) -> bool {
        matches!(self, Self::Instruction | Self::Unified)
    }
}

/// One distinct kernel-reported cache instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTopology {
    level: usize,
    cache_type: CacheType,
    id: Option<usize>,
    shared_cpus: Vec<CpuId>,
    size_bytes: Option<u64>,
}

impl CacheTopology {
    pub fn level(&self) -> usize {
        self.level
    }

    pub fn cache_type(&self) -> CacheType {
        self.cache_type
    }

    /// Kernel cache-instance ID, when exposed.
    pub fn id(&self) -> Option<usize> {
        self.id
    }

    /// Logical CPUs that share this cache instance.
    pub fn shared_cpus(&self) -> &[CpuId] {
        &self.shared_cpus
    }

    pub fn size_bytes(&self) -> Option<u64> {
        self.size_bytes
    }
}

/// Kernel-reported topology domains for one logical CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuTopology {
    cpu_id: CpuId,
    core_cpus: Vec<CpuId>,
    package_cpus: Vec<CpuId>,
    die_cpus: Option<Vec<CpuId>>,
    numa_node_id: Option<usize>,
    cache_indices: Vec<usize>,
}

impl CpuTopology {
    pub fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }

    /// Hardware threads in the same physical core.
    pub fn core_cpus(&self) -> &[CpuId] {
        &self.core_cpus
    }

    /// Logical CPUs in the same physical package.
    pub fn package_cpus(&self) -> &[CpuId] {
        &self.package_cpus
    }

    /// Logical CPUs in the same die, when Linux exposes a die domain.
    pub fn die_cpus(&self) -> Option<&[CpuId]> {
        self.die_cpus.as_deref()
    }

    /// NUMA node containing this CPU, when NUMA topology is exposed.
    pub fn numa_node_id(&self) -> Option<usize> {
        self.numa_node_id
    }
}

/// NUMA node membership and relative distance to other online nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaNode {
    id: usize,
    cpus: Vec<CpuId>,
    distances: Vec<(usize, u32)>,
}

impl NumaNode {
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn cpus(&self) -> &[CpuId] {
        &self.cpus
    }

    /// `(node_id, relative_distance)` pairs in kernel node-list order.
    pub fn distances(&self) -> impl ExactSizeIterator<Item = (usize, u32)> + '_ {
        self.distances.iter().copied()
    }

    pub fn distance_to(&self, node_id: usize) -> Option<u32> {
        self.distances
            .iter()
            .find_map(|&(id, distance)| (id == node_id).then_some(distance))
    }
}

/// Immutable snapshot of kernel-reported CPU, cache, and NUMA topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTopology {
    cpus: Vec<CpuTopology>,
    caches: Vec<CacheTopology>,
    numa_nodes: Vec<NumaNode>,
    cpu_index: Vec<Option<usize>>,
}

impl SystemTopology {
    /// Per-CPU topology in the order supplied to [`discover_topology`].
    pub fn cpus(&self) -> &[CpuTopology] {
        &self.cpus
    }

    /// Every distinct cache instance referenced by the requested CPUs.
    pub fn caches(&self) -> &[CacheTopology] {
        &self.caches
    }

    /// Every online NUMA node reported by Linux.
    pub fn numa_nodes(&self) -> &[NumaNode] {
        &self.numa_nodes
    }

    pub fn cpu(&self, cpu: CpuId) -> Option<&CpuTopology> {
        self.cpu_index
            .get(*cpu)
            .and_then(|index| index.and_then(|index| self.cpus.get(index)))
    }

    pub fn numa_node(&self, node_id: usize) -> Option<&NumaNode> {
        self.numa_nodes.iter().find(|node| node.id == node_id)
    }

    /// Cache hierarchy for `cpu`, ordered by the kernel's numeric `indexN` enumeration.
    pub fn caches_for(&self, cpu: CpuId) -> Option<impl ExactSizeIterator<Item = &CacheTopology>> {
        let cpu = self.cpu(cpu)?;
        Some(cpu.cache_indices.iter().map(|&index| &self.caches[index]))
    }

    /// Unique highest-level data-capable cache for `cpu`.
    ///
    /// Returns `None` when the CPU is absent, no data-capable cache is known, or multiple distinct
    /// caches exist at the highest data-cache level.
    pub fn data_llc(&self, cpu: CpuId) -> Option<&CacheTopology> {
        let mut highest_level = None;
        let mut result = None;
        let mut ambiguous = false;

        for cache in self
            .caches_for(cpu)?
            .filter(|cache| cache.cache_type.supports_data())
        {
            match highest_level {
                Some(level) if cache.level < level => {}
                Some(level) if cache.level == level => ambiguous = true,
                _ => {
                    highest_level = Some(cache.level);
                    result = Some(cache);
                    ambiguous = false;
                }
            }
        }

        (!ambiguous).then_some(result).flatten()
    }
}

/// Discover kernel-reported topology for the supplied logical CPUs.
///
/// Discovery performs blocking sysfs I/O and heap allocation and is intended for initialization,
/// not latency-sensitive runtime paths. `SystemTopology::cpus()` preserves input order. Duplicate
/// input CPU IDs are rejected.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> std::io::Result<()> {
/// let allowed = cpu_affinity(None)?;
/// let topology = discover_topology(allowed)?;
/// for cpu in topology.cpus() {
///     println!("CPU {:?} shares a core with {:?}", cpu.cpu_id(), cpu.core_cpus());
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`io::Error`] if reading or parsing kernel sysfs topology fails, or if the input contains
/// a duplicate CPU ID.
pub fn discover_topology(cpus: impl IntoIterator<Item = CpuId>) -> io::Result<SystemTopology> {
    discover_topology_at(Path::new(CPU_SYSFS_PATH), Path::new(NODE_SYSFS_PATH), cpus)
}

/// Get all online logical CPUs from sysfs.
///
/// # Errors
///
/// Returns [`io::Error`] if reading or parsing the kernel sysfs CPU list fails.
pub fn online_cpus() -> io::Result<Vec<CpuId>> {
    read_cpu_list(Path::new(CPU_SYSFS_PATH).join("online"))
}

fn discover_topology_at(
    root: &Path,
    node_root: &Path,
    cpus: impl IntoIterator<Item = CpuId>,
) -> io::Result<SystemTopology> {
    let numa_nodes = read_numa_nodes(node_root)?;
    let mut node_of_cpu = HashMap::new();
    for node in &numa_nodes {
        node_of_cpu.extend(node.cpus.iter().map(|&cpu| (cpu, node.id)));
    }

    let mut caches = Vec::new();
    let mut cache_keys = HashMap::new();
    let mut topology_cpus = Vec::new();
    let mut cpu_index = Vec::new();

    for cpu_id in cpus {
        if cpu_index.get(*cpu_id).is_some_and(Option::is_some) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }

        let cache_indices = read_cache_topology(root, cpu_id)?
            .into_iter()
            .map(|cache| intern_cache(&mut caches, &mut cache_keys, cache))
            .collect();
        let index = topology_cpus.len();

        topology_cpus.push(CpuTopology {
            cpu_id,
            core_cpus: read_topology_cpu_list_preferred(
                root,
                cpu_id,
                "core_cpus_list",
                "thread_siblings_list",
            )?,
            package_cpus: read_topology_cpu_list_preferred(
                root,
                cpu_id,
                "package_cpus_list",
                "core_siblings_list",
            )?,
            die_cpus: read_optional_topology_cpu_list(root, cpu_id, "die_cpus_list")?,
            numa_node_id: node_of_cpu.get(&cpu_id).copied(),
            cache_indices,
        });

        let cpu_index_len = (*cpu_id)
            .checked_add(1)
            .ok_or_else(|| invalid_data("CPU index length overflow"))?;
        if cpu_index.len() < cpu_index_len {
            cpu_index.resize(cpu_index_len, None);
        }
        cpu_index[*cpu_id] = Some(index);
    }

    Ok(SystemTopology {
        cpus: topology_cpus,
        caches,
        numa_nodes,
        cpu_index,
    })
}

#[derive(PartialEq, Eq, Hash)]
enum CacheKey {
    Id(usize, CacheType, usize),
    Sharing(usize, CacheType, Vec<CpuId>),
}

fn intern_cache(
    caches: &mut Vec<CacheTopology>,
    keys: &mut HashMap<CacheKey, usize>,
    cache: CacheTopology,
) -> usize {
    let key = match cache.id {
        Some(id) => CacheKey::Id(cache.level, cache.cache_type, id),
        None => CacheKey::Sharing(cache.level, cache.cache_type, cache.shared_cpus.clone()),
    };

    *keys.entry(key).or_insert_with(|| {
        let index = caches.len();
        caches.push(cache);
        index
    })
}

fn cpu_dir(root: &Path, cpu_id: CpuId) -> PathBuf {
    root.join(format!("cpu{}", *cpu_id))
}

fn read_topology_cpu_list(root: &Path, cpu_id: CpuId, field: &str) -> io::Result<Vec<CpuId>> {
    read_cpu_list(cpu_dir(root, cpu_id).join("topology").join(field))
}

fn read_optional_topology_cpu_list(
    root: &Path,
    cpu_id: CpuId,
    field: &str,
) -> io::Result<Option<Vec<CpuId>>> {
    match read_topology_cpu_list(root, cpu_id, field) {
        Ok(list) => Ok(Some(list)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_topology_cpu_list_preferred(
    root: &Path,
    cpu_id: CpuId,
    preferred: &str,
    fallback: &str,
) -> io::Result<Vec<CpuId>> {
    match read_topology_cpu_list(root, cpu_id, preferred) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            read_topology_cpu_list(root, cpu_id, fallback)
        }
        result => result,
    }
}

fn read_numa_nodes(node_root: &Path) -> io::Result<Vec<NumaNode>> {
    let online_path = node_root.join("online");
    let node_ids = match read_parsed(&online_path, parse_id_list) {
        Ok(node_ids) => node_ids,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    node_ids
        .iter()
        .copied()
        .map(|id| {
            let node_dir = node_root.join(format!("node{id}"));
            let distance_path = node_dir.join("distance");
            let distances = read_parsed(&distance_path, parse_numa_distances)?;
            if distances.len() != node_ids.len() {
                return Err(invalid_data("invalid sysfs NUMA distance count"));
            }

            Ok(NumaNode {
                id,
                cpus: read_cpu_list(node_dir.join("cpulist"))?,
                distances: node_ids.iter().copied().zip(distances).collect(),
            })
        })
        .collect()
}

fn read_cache_topology(root: &Path, cpu_id: CpuId) -> io::Result<Vec<CacheTopology>> {
    let cache_dir = cpu_dir(root, cpu_id).join("cache");
    let mut caches = Vec::new();

    for entry in fs::read_dir(&cache_dir)? {
        let path = entry?.path();
        let Some(index) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("index"))
        else {
            continue;
        };
        let index = parse_topology_id(index)?;
        let cache_type_value = read_trimmed(path.join("type"))?;
        let cache_type = match cache_type_value.as_str() {
            "Data" => CacheType::Data,
            "Instruction" => CacheType::Instruction,
            "Unified" => CacheType::Unified,
            _ => return Err(invalid_data("invalid sysfs cache type")),
        };

        caches.push((
            index,
            CacheTopology {
                level: read_usize(path.join("level"))?,
                cache_type,
                id: read_optional_usize(path.join("id"))?,
                shared_cpus: read_cpu_list(path.join("shared_cpu_list"))?,
                size_bytes: read_optional_cache_size(path.join("size"))?,
            },
        ));
    }

    caches.sort_unstable_by_key(|(index, _)| *index);
    Ok(caches.into_iter().map(|(_, cache)| cache).collect())
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    fs::read_to_string(path).map(|value| value.trim().to_string())
}

fn read_parsed<T>(
    path: impl AsRef<Path>,
    parse: impl FnOnce(&str) -> io::Result<T>,
) -> io::Result<T> {
    let value = read_trimmed(path)?;
    parse(&value)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_topology_id(value: &str) -> io::Result<usize> {
    value
        .trim()
        .parse()
        .map_err(|_| invalid_data("invalid sysfs topology integer"))
}

fn read_usize(path: impl AsRef<Path>) -> io::Result<usize> {
    read_parsed(path, parse_topology_id)
}

fn read_optional_usize(path: impl AsRef<Path>) -> io::Result<Option<usize>> {
    match read_usize(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_optional_cache_size(path: impl AsRef<Path>) -> io::Result<Option<u64>> {
    match read_parsed(path, parse_cache_size) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_cache_size(value: &str) -> io::Result<u64> {
    let (digits, multiplier) = match value.strip_suffix('K') {
        Some(digits) => (digits, 1024u64),
        None => match value.strip_suffix('M') {
            Some(digits) => (digits, 1024 * 1024),
            None => (value, 1),
        },
    };

    digits
        .parse::<u64>()
        .map_err(|_| invalid_data("invalid sysfs cache size"))?
        .checked_mul(multiplier)
        .ok_or_else(|| invalid_data("sysfs cache size overflow"))
}

fn read_cpu_list(path: impl AsRef<Path>) -> io::Result<Vec<CpuId>> {
    read_parsed(path, parse_cpu_list)
}

fn parse_id_list(value: &str) -> io::Result<Vec<usize>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    for part in value.trim().split(',') {
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (parse_topology_id(start)?, parse_topology_id(end)?),
            None => {
                let id = parse_topology_id(part)?;
                (id, id)
            }
        };
        let count = end
            .checked_sub(start)
            .and_then(|range| range.checked_add(1))
            .ok_or_else(|| invalid_data("invalid sysfs ID range"))?;
        let expanded_len = ids
            .len()
            .checked_add(count)
            .ok_or_else(|| invalid_data("sysfs ID list exceeds CPU_SETSIZE"))?;
        if expanded_len > CPU_SETSIZE {
            return Err(invalid_data("sysfs ID list exceeds CPU_SETSIZE"));
        }
        ids.extend(start..=end);
    }

    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<CpuId>> {
    parse_id_list(value)?
        .into_iter()
        .map(|id| CpuId::new(id).map_err(|_| invalid_data("sysfs CPU id exceeds CPU_SETSIZE")))
        .collect()
}

fn parse_numa_distances(value: &str) -> io::Result<Vec<u32>> {
    value
        .split_whitespace()
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid_data("invalid sysfs NUMA distance"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use {super::*, std::fs::create_dir_all};

    #[test]
    fn test_parse_topology_id() {
        assert_eq!(parse_topology_id("42\n").unwrap(), 42);
        assert_error_kind(
            parse_topology_id("not-a-number"),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_parse_cpu_list() {
        assert_eq!(parse_cpu_list("0,2-4\n").unwrap(), cpu_ids([0, 2, 3, 4]));
        assert_eq!(parse_cpu_list("3,1-3").unwrap(), cpu_ids([1, 2, 3]));
        assert_eq!(parse_cpu_list("").unwrap(), Vec::new());
        assert_error_kind(parse_cpu_list("4-2"), io::ErrorKind::InvalidData);
        assert_error_kind(
            parse_cpu_list(&format!("0-{CPU_SETSIZE}")),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_parse_cache_size() {
        assert_eq!(parse_cache_size("32K").unwrap(), 32 * 1024);
        assert_eq!(parse_cache_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_cache_size("4096").unwrap(), 4096);
        assert_error_kind(parse_cache_size("bogus"), io::ErrorKind::InvalidData);
        assert_error_kind(
            parse_cache_size(&format!("{}M", u64::MAX)),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_intern_cache_deduplicates_by_level_type_id() {
        let mut caches = Vec::new();
        let mut keys = HashMap::new();
        let cache = |level, cache_type, id| CacheTopology {
            level,
            cache_type,
            id: Some(id),
            shared_cpus: cpu_ids([0, 1]),
            size_bytes: None,
        };

        let a = intern_cache(&mut caches, &mut keys, cache(3, CacheType::Unified, 0));
        let b = intern_cache(&mut caches, &mut keys, cache(3, CacheType::Unified, 0));
        let c = intern_cache(&mut caches, &mut keys, cache(2, CacheType::Unified, 0));

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(caches.len(), 2);
    }

    fn write_cpu_fixture(root: &Path, cpu: usize, sibling: usize) {
        let cpu_dir = root.join(format!("cpu{cpu}"));
        let topology_dir = cpu_dir.join("topology");
        create_dir_all(&topology_dir).unwrap();
        fs::write(topology_dir.join("core_cpus_list"), cpu.to_string()).unwrap();
        fs::write(topology_dir.join("package_cpus_list"), "0-1").unwrap();

        let write_cache =
            |index: usize, level: usize, cache_type: &str, id: usize, shared: &str| {
                let index_dir = cpu_dir.join("cache").join(format!("index{index}"));
                create_dir_all(&index_dir).unwrap();
                fs::write(index_dir.join("level"), level.to_string()).unwrap();
                fs::write(index_dir.join("type"), cache_type).unwrap();
                fs::write(index_dir.join("id"), id.to_string()).unwrap();
                fs::write(index_dir.join("shared_cpu_list"), shared).unwrap();
                fs::write(index_dir.join("size"), "32K").unwrap();
            };
        write_cache(0, 1, "Data", cpu, &cpu.to_string());
        write_cache(1, 1, "Instruction", cpu, &cpu.to_string());
        write_cache(2, 2, "Unified", cpu, &cpu.to_string());
        write_cache(3, 3, "Unified", 0, &format!("{cpu},{sibling}"));
    }

    #[test]
    fn test_discover_topology_at_fixture() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();

        write_cpu_fixture(root.path(), 0, 1);
        write_cpu_fixture(root.path(), 1, 0);

        let topology =
            discover_topology_at(root.path(), node_root.path(), cpu_ids([0, 1])).unwrap();

        assert_eq!(topology.cpus().len(), 2);
        assert_eq!(topology.cpu(cpu(0)).unwrap().cpu_id(), cpu(0));
        assert_eq!(topology.cpu(cpu(0)).unwrap().core_cpus(), &[cpu(0)]);
        assert_eq!(
            topology.cpu(cpu(0)).unwrap().package_cpus(),
            &[cpu(0), cpu(1)]
        );
        assert_eq!(topology.cpu(cpu(0)).unwrap().die_cpus(), None);
        assert!(topology.numa_nodes().is_empty());

        // Four distinct caches per CPU, with the shared L3 stored once.
        assert_eq!(topology.caches().len(), 7);
        let cpu0_caches = topology.caches_for(cpu(0)).unwrap().collect::<Vec<_>>();
        assert_eq!(cpu0_caches.len(), 4);
        assert_eq!(cpu0_caches[0].cache_type(), CacheType::Data);
        assert_eq!(cpu0_caches[1].cache_type(), CacheType::Instruction);
        assert_eq!(topology.data_llc(cpu(0)), topology.data_llc(cpu(1)));
        assert_eq!(topology.data_llc(cpu(0)).unwrap().level(), 3);
        assert_eq!(
            topology.data_llc(cpu(0)).unwrap().size_bytes(),
            Some(32 * 1024)
        );
    }

    #[test]
    fn test_discover_topology_at_sparse_numa_nodes() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();

        write_cpu_fixture(root.path(), 0, 1);
        write_cpu_fixture(root.path(), 1, 0);

        fs::write(node_root.path().join("online"), "0,2").unwrap();
        for (node, cpulist, distance) in [(0, "0", "10 21"), (2, "1", "21 10")] {
            let node_dir = node_root.path().join(format!("node{node}"));
            create_dir_all(&node_dir).unwrap();
            fs::write(node_dir.join("cpulist"), cpulist).unwrap();
            fs::write(node_dir.join("distance"), distance).unwrap();
        }

        let topology =
            discover_topology_at(root.path(), node_root.path(), cpu_ids([0, 1])).unwrap();

        assert_eq!(topology.cpu(cpu(0)).unwrap().numa_node_id(), Some(0));
        assert_eq!(topology.cpu(cpu(1)).unwrap().numa_node_id(), Some(2));
        assert_eq!(topology.numa_node(0).unwrap().distance_to(2), Some(21));
        assert_eq!(
            topology
                .numa_node(2)
                .unwrap()
                .distances()
                .collect::<Vec<_>>(),
            vec![(0, 21), (2, 10)]
        );
    }

    #[test]
    fn test_topology_io_error_propagates() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), [cpu(0)]),
            io::ErrorKind::NotFound,
        );
    }

    #[test]
    fn test_invalid_cache_type_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);
        let type_path = root.path().join("cpu0/cache/index0/type");
        fs::write(&type_path, "Bogus").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), [cpu(0)]),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_invalid_integer_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);
        let level_path = root.path().join("cpu0/cache/index0/level");
        fs::write(&level_path, "Bogus").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), [cpu(0)]),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_invalid_cache_size_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);
        let size_path = root.path().join("cpu0/cache/index0/size");
        fs::write(&size_path, "Bogus").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), [cpu(0)]),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_missing_optional_cache_fields_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);
        let cache_path = root.path().join("cpu0/cache/index0");
        fs::remove_file(cache_path.join("id")).unwrap();
        fs::remove_file(cache_path.join("size")).unwrap();

        let topology = discover_topology_at(root.path(), node_root.path(), [cpu(0)]).unwrap();
        let cache = topology.caches_for(cpu(0)).unwrap().next().unwrap();

        assert_eq!(cache.id(), None);
        assert_eq!(cache.size_bytes(), None);
    }

    #[test]
    fn test_invalid_cpu_list_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);
        let shared_cpu_path = root.path().join("cpu0/cache/index0/shared_cpu_list");
        fs::write(&shared_cpu_path, "4-2").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), [cpu(0)]),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_invalid_numa_distance_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        fs::write(node_root.path().join("online"), "0").unwrap();
        let node_dir = node_root.path().join("node0");
        create_dir_all(&node_dir).unwrap();
        let distance_path = node_dir.join("distance");
        fs::write(&distance_path, "bogus").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), std::iter::empty::<CpuId>()),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_invalid_numa_distance_count_is_invalid_data() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        fs::write(node_root.path().join("online"), "0,2").unwrap();
        let node_dir = node_root.path().join("node0");
        create_dir_all(&node_dir).unwrap();
        let distance_path = node_dir.join("distance");
        fs::write(&distance_path, "10").unwrap();

        assert_error_kind(
            discover_topology_at(root.path(), node_root.path(), std::iter::empty::<CpuId>()),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn test_discover_topology_rejects_duplicate_cpu() {
        let root = tempfile::tempdir().unwrap();
        let node_root = tempfile::tempdir().unwrap();
        write_cpu_fixture(root.path(), 0, 0);

        let error =
            discover_topology_at(root.path(), node_root.path(), [cpu(0), cpu(0)]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_discover_topology_preserves_input_order() {
        let cpus = crate::cpu_affinity(None).expect("failed to query current CPU affinity");
        let cpus = cpus.into_iter().take(2).collect::<Vec<_>>();

        let topology =
            discover_topology(cpus.iter().copied()).expect("failed to query CPU topology");

        assert!(
            topology
                .cpus()
                .iter()
                .map(CpuTopology::cpu_id)
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

    fn cpu(id: usize) -> CpuId {
        CpuId::new(id).unwrap()
    }

    fn cpu_ids(cpus: impl IntoIterator<Item = usize>) -> Vec<CpuId> {
        cpus.into_iter().map(cpu).collect()
    }

    fn assert_error_kind<T>(result: io::Result<T>, expected: io::ErrorKind) {
        match result {
            Ok(_) => panic!("expected {expected:?} error"),
            Err(error) => assert_eq!(error.kind(), expected),
        }
    }
}
