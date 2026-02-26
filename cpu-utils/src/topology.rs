//! CPU topology detection and physical core management.

#[cfg(target_os = "linux")]
use {
    crate::affinity::{cpu_count, max_cpu_id, set_cpu_affinity},
    std::{collections::HashSet, fs},
};
use {crate::error::CpuAffinityError, std::collections::BTreeMap};

/// Get the number of physical CPU cores (excluding hyperthreads).
///
/// Reads CPU topology from sysfs to count unique physical cores. If topology
/// information is unavailable, assumes no hyperthreading and returns the total CPU count.
///
/// # Returns
/// The count of physical cores on the system.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let physical = physical_core_count()?;
/// let logical = cpu_count()?;
/// println!("Physical cores: {}, Logical CPUs: {}", physical, logical);
/// if logical > physical {
///     println!("Hyperthreading enabled ({}x)", logical / physical);
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::Io`] if unable to read system information.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn physical_core_count() -> Result<usize, CpuAffinityError> {
    let max_cpu = max_cpu_id()?;
    let mut seen_cores = HashSet::new();

    for cpu in 0..=max_cpu {
        let core_id_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id");

        if let Ok(content) = fs::read_to_string(&core_id_path) {
            if let Ok(core_id) = content.trim().parse::<usize>() {
                // Sanity check:
                // We use max_cpu * 2 as an upper bound because:
                // - Core IDs are typically sequential from 0
                // - Even with NUMA and multiple sockets, core_id rarely exceeds max_cpu
                // - This guards against corrupted sysfs data
                // - Factor of 2 provides margin for unusual topologies
                if core_id <= max_cpu.saturating_mul(2) {
                    seen_cores.insert(core_id);
                }
            }
        }
    }

    if seen_cores.is_empty() {
        // Fallback: assume no hyperthreading
        cpu_count()
    } else {
        Ok(seen_cores.len())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn physical_core_count() -> Result<usize, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get a mapping of physical core IDs to logical CPU IDs.
///
/// Useful for understanding CPU topology and identifying hyperthreading siblings.
///
/// # Returns
/// A BTreeMap where keys are physical core IDs and values are sorted vectors of
/// logical CPU IDs that belong to each physical core.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let mapping = core_to_cpus_mapping()?;
/// for (core_id, cpus) in &mapping {
///     println!("Core {} -> CPUs {:?}", core_id, cpus);
/// }
/// # Ok(())
/// # }
/// ```
///
/// On a system with hyperthreading, core 0 might map to CPUs `[0, 4]`.
///
/// # Errors
///
/// Returns [`CpuAffinityError::Io`] if unable to read topology information.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn core_to_cpus_mapping() -> Result<BTreeMap<usize, Vec<usize>>, CpuAffinityError> {
    let max_cpu = max_cpu_id()?;
    let mut mapping: BTreeMap<usize, Vec<usize>> = BTreeMap::new();

    for cpu in 0..=max_cpu {
        let core_id_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id");

        if let Ok(content) = fs::read_to_string(&core_id_path) {
            if let Ok(core_id) = content.trim().parse::<usize>() {
                // Sanity check: same reasoning as in physical_core_count()
                // Guards against corrupted sysfs data while allowing for unusual topologies
                if core_id <= max_cpu.saturating_mul(2) {
                    mapping.entry(core_id).or_default().push(cpu);
                }
            }
        }
    }

    // Sort CPU lists for consistency
    for cpus in mapping.values_mut() {
        cpus.sort_unstable();
    }

    Ok(mapping)
}

#[cfg(not(target_os = "linux"))]
pub fn core_to_cpus_mapping() -> Result<BTreeMap<usize, Vec<usize>>, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Set CPU affinity using only physical cores (avoiding hyperthreads).
///
/// Pins the thread to the first logical CPU of each specified physical core,
/// avoiding hyperthreading siblings. Useful for reducing performance variance
/// and cache contention in performance-critical threads.
///
/// # Arguments
/// * `cores` - Physical core IDs to bind to
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// // Pin to physical cores 0 and 1 (avoids hyperthreads)
/// set_affinity_physical_cores_only([0, 1])?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::EmptyCpuList`] if the core list is empty.
/// Returns [`CpuAffinityError::InvalidPhysicalCore`] if any core ID is invalid.
/// Returns [`CpuAffinityError::Io`] if the system call fails.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
///
#[cfg(target_os = "linux")]
pub fn set_affinity_physical_cores_only(
    cores: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    let cores: HashSet<usize> = cores.into_iter().collect();

    if cores.is_empty() {
        return Err(CpuAffinityError::EmptyCpuList);
    }

    let mapping = core_to_cpus_mapping()?;
    let max_core = mapping.keys().max().copied().unwrap_or(0);

    // Validate core IDs
    for &core in &cores {
        if !mapping.contains_key(&core) {
            return Err(CpuAffinityError::InvalidPhysicalCore {
                core,
                max: max_core,
            });
        }
    }

    // Collect the first CPU of each physical core
    let mut cpus = Vec::new();
    for core in cores {
        if let Some(core_cpus) = mapping.get(&core) {
            if let Some(&first_cpu) = core_cpus.first() {
                cpus.push(first_cpu);
            }
        }
    }

    cpus.sort_unstable();
    set_cpu_affinity(cpus)
}

#[cfg(not(target_os = "linux"))]
pub fn set_affinity_physical_cores_only(
    _cores: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_physical_core_count() {
        match physical_core_count() {
            Ok(count) => {
                assert!(count > 0, "Should have at least one physical core");

                // Physical cores should be <= total CPUs
                if let Ok(cpu_count) = crate::affinity::cpu_count() {
                    assert!(
                        count <= cpu_count,
                        "Physical cores ({count}) should not exceed total CPUs ({cpu_count})"
                    );
                }
            }
            Err(CpuAffinityError::NotSupported) => {
                // Expected on non-Linux platforms
            }
            Err(e) => panic!("Unexpected error: {e:?}"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_core_to_cpus_mapping_consistency() {
        if let Ok(mapping) = core_to_cpus_mapping() {
            // Each core should have at least one CPU
            for (core_id, cpus) in &mapping {
                assert!(
                    !cpus.is_empty(),
                    "Core {core_id} should have at least one CPU"
                );

                // CPUs should be sorted
                let mut sorted = cpus.clone();
                sorted.sort_unstable();
                assert_eq!(cpus, &sorted, "CPUs for core {core_id} should be sorted");
            }

            // Total CPUs in mapping should not exceed system CPU count
            if let Ok(total_cpus) = cpu_count() {
                let mapped_cpus: HashSet<_> = mapping.values().flat_map(|v| v.iter()).collect();
                assert!(
                    mapped_cpus.len() <= total_cpus,
                    "Mapped CPUs count ({}) should not exceed total CPUs ({})",
                    mapped_cpus.len(),
                    total_cpus
                );
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_core_to_cpus_no_duplicates() {
        if let Ok(mapping) = core_to_cpus_mapping() {
            let mut all_cpus: Vec<usize> = Vec::new();
            for cpus in mapping.values() {
                all_cpus.extend(cpus);
            }

            let unique_cpus: HashSet<_> = all_cpus.iter().collect();
            assert_eq!(
                all_cpus.len(),
                unique_cpus.len(),
                "Each CPU should belong to exactly one core"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_set_affinity_physical_cores_validation() {
        // Test empty list
        assert!(matches!(
            set_affinity_physical_cores_only([]).unwrap_err(),
            CpuAffinityError::EmptyCpuList
        ));

        // Test with invalid core ID (very high number)
        let result = set_affinity_physical_cores_only([99999]);
        assert!(matches!(
            result.unwrap_err(),
            CpuAffinityError::InvalidPhysicalCore { .. }
        ));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_not_supported_on_non_linux() {
        assert!(matches!(
            physical_core_count().unwrap_err(),
            CpuAffinityError::NotSupported
        ));
        assert!(matches!(
            core_to_cpus_mapping().unwrap_err(),
            CpuAffinityError::NotSupported
        ));
        assert!(matches!(
            set_affinity_physical_cores_only([0]).unwrap_err(),
            CpuAffinityError::NotSupported
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_physical_vs_logical_consistency() {
        // Physical cores * threads per core should roughly equal total CPUs
        if let (Ok(physical), Ok(total)) = (physical_core_count(), cpu_count()) {
            if physical > 0 {
                let threads_per_core = total.div_ceil(physical);
                assert!(
                    (1..=4).contains(&threads_per_core),
                    "Threads per core ({threads_per_core}) should be between 1 and 4"
                );
            }
        }
    }
}
