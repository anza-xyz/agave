//! Core CPU affinity operations.

use crate::error::CpuAffinityError;
use std::collections::HashSet;
use std::fs;
use std::io;

/// Maximum CPU ID that can be used with CPU_SET.
///
/// This is the standard Linux value defined in glibc. While this could theoretically
/// vary on different systems, 1024 is the standard value used across all major
/// Linux distributions. The kernel itself supports more CPUs, but the cpu_set_t
/// structure in glibc is fixed at this size.
#[cfg(target_os = "linux")]
const CPU_SETSIZE: usize = 1024;

/// Set CPU affinity for the calling thread.
///
/// Restricts the thread to run only on the specified CPUs. Duplicate CPU IDs are
/// automatically deduplicated.
///
/// # Arguments
/// * `cpus` - Iterator of CPU IDs to bind the thread to
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// // Pin to CPU 0
/// set_cpu_affinity([0])?;
///
/// // Pin to multiple CPUs
/// set_cpu_affinity([0, 1, 2])?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::EmptyCpuList`] if the CPU list is empty.
/// Returns [`CpuAffinityError::InvalidCpu`] if any CPU ID exceeds the system maximum.
/// Returns [`CpuAffinityError::SystemCall`] if the system call fails (e.g., permission denied).
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
///
#[cfg(target_os = "linux")]
pub fn set_cpu_affinity(
    cpus: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    // Convert to HashSet to remove duplicates
    let cpus: HashSet<usize> = cpus.into_iter().collect();

    if cpus.is_empty() {
        return Err(CpuAffinityError::EmptyCpuList);
    }

    // Validate CPU IDs
    let max_cpu = max_cpu_id()?;
    for &cpu in &cpus {
        if cpu > max_cpu {
            return Err(CpuAffinityError::InvalidCpu { cpu, max: max_cpu });
        }
        // Also validate against CPU_SETSIZE to prevent undefined behavior
        if cpu >= CPU_SETSIZE {
            return Err(CpuAffinityError::InvalidCpu {
                cpu,
                max: CPU_SETSIZE - 1,
            });
        }
    }

    // Initialize CPU set
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // Add CPUs to the set
    for cpu in cpus {
        // safety: We've validated cpu is within valid range
        unsafe {
            libc::CPU_SET(cpu, &mut cpu_set);
        }
    }

    // Apply the affinity
    // safety: sched_setaffinity is safe with valid parameters
    let result = unsafe {
        libc::sched_setaffinity(
            0, // 0 means current thread
            std::mem::size_of::<libc::cpu_set_t>(),
            &cpu_set,
        )
    };

    if result != 0 {
        return Err(CpuAffinityError::SystemCall(
            io::Error::last_os_error().to_string(),
        ));
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_cpu_affinity(
    _cpus: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the CPU affinity mask for the calling thread.
///
/// Returns a sorted vector of CPU IDs that the thread is allowed to run on.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let cpus = cpu_affinity()?;
/// println!("Thread can run on CPUs: {:?}", cpus);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::SystemCall`] if the system call fails.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn cpu_affinity() -> Result<Vec<usize>, CpuAffinityError> {
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // Get current affinity
    // safety: sched_getaffinity is safe with valid parameters
    let result = unsafe {
        libc::sched_getaffinity(
            0, // 0 means current thread
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut cpu_set,
        )
    };

    if result != 0 {
        return Err(CpuAffinityError::SystemCall(
            io::Error::last_os_error().to_string(),
        ));
    }

    // Extract CPU IDs from the set
    let max_cpu = max_cpu_id()?;
    let mut cpus = Vec::new();

    for cpu in 0..=max_cpu {
        // safety: CPU_ISSET is safe with valid cpu_set_t and cpu < CPU_SETSIZE
        let is_set = unsafe { libc::CPU_ISSET(cpu, &cpu_set) };
        if is_set {
            cpus.push(cpu);
        }
    }

    Ok(cpus)
}

#[cfg(not(target_os = "linux"))]
pub fn cpu_affinity() -> Result<Vec<usize>, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the maximum CPU ID on the system.
///
/// Reads from `/sys/devices/system/cpu/present` or falls back to `sysconf(_SC_NPROCESSORS_CONF)`.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let max = max_cpu_id()?;
/// println!("Valid CPU range: 0-{}", max);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::SystemCall`] if unable to determine CPU count.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn max_cpu_id() -> Result<usize, CpuAffinityError> {
    // Try to read from sysfs first
    if let Ok(content) = fs::read_to_string("/sys/devices/system/cpu/present") {
        let content = content.trim();

        // Parse range (e.g., "0-127" or just "0")
        if let Some(range) = content.split('-').nth(1) {
            if let Ok(max) = range.parse::<usize>() {
                return Ok(max);
            }
        } else if let Ok(max) = content.parse::<usize>() {
            return Ok(max);
        }
    }

    // Fallback to sysconf
    // safety: sysconf is safe to call
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };

    if count <= 0 {
        return Err(CpuAffinityError::SystemCall(
            "Failed to get processor count".to_string(),
        ));
    }

    Ok((count as usize).saturating_sub(1))
}

#[cfg(not(target_os = "linux"))]
pub fn max_cpu_id() -> Result<usize, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the total number of CPUs on the system.
///
/// Returns the count of logical CPUs (includes hyperthreads). Equivalent to `max_cpu_id() + 1`.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let count = cpu_count()?;
/// println!("System has {} logical CPUs", count);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::SystemCall`] if unable to determine CPU count.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
pub fn cpu_count() -> Result<usize, CpuAffinityError> {
    Ok(max_cpu_id()? + 1)
}

/// Get the list of isolated CPUs.
///
/// Isolated CPUs are those reserved via kernel boot parameters (`isolcpus=...`)
/// for low-latency or real-time workloads. The kernel scheduler avoids placing
/// regular tasks on these CPUs.
///
/// # Returns
/// A sorted vector of isolated CPU IDs, or an empty vector if none are isolated.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let isolated = isolated_cpus()?;
/// if !isolated.is_empty() {
///     println!("Isolated CPUs: {:?}", isolated);
///     set_cpu_affinity([isolated[0]])?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::ParseError`] if the sysfs data is malformed.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn isolated_cpus() -> Result<Vec<usize>, CpuAffinityError> {
    match fs::read_to_string("/sys/devices/system/cpu/isolated") {
        Ok(content) => {
            let content = content.trim();
            if content.is_empty() {
                return Ok(Vec::new());
            }
            parse_cpu_range_list(&content)
        }
        Err(_) => {
            // File doesn't exist or can't be read - no isolated CPUs
            Ok(Vec::new())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn isolated_cpus() -> Result<Vec<usize>, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Parse a CPU range list string (e.g., "0-3,5,7-9") into a vector of CPU IDs.
#[cfg(target_os = "linux")]
fn parse_cpu_range_list(s: &str) -> Result<Vec<usize>, CpuAffinityError> {
    let mut cpus = HashSet::new();

    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(dash_pos) = part.find('-') {
            // Range (e.g., "0-3")
            let start = part[..dash_pos]
                .trim()
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU range: {}", part)))?;
            let end = part[dash_pos + 1..]
                .trim()
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU range: {}", part)))?;

            for cpu in start..=end {
                cpus.insert(cpu);
            }
        } else {
            // Single CPU
            let cpu = part
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU ID: {}", part)))?;
            cpus.insert(cpu);
        }
    }

    let mut result: Vec<usize> = cpus.into_iter().collect();
    result.sort_unstable();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn test_parse_cpu_range_list() {
        // Test single CPU
        assert_eq!(parse_cpu_range_list("0").unwrap(), vec![0]);
        assert_eq!(parse_cpu_range_list("5").unwrap(), vec![5]);

        // Test ranges
        assert_eq!(parse_cpu_range_list("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_range_list("5-7").unwrap(), vec![5, 6, 7]);

        // Test mixed single and ranges
        assert_eq!(
            parse_cpu_range_list("0-2,5,7-9").unwrap(),
            vec![0, 1, 2, 5, 7, 8, 9]
        );

        // Test with spaces
        assert_eq!(
            parse_cpu_range_list(" 0 - 2 , 5 , 7 - 9 ").unwrap(),
            vec![0, 1, 2, 5, 7, 8, 9]
        );

        // Test duplicates are removed
        assert_eq!(
            parse_cpu_range_list("0,1,0,2,1").unwrap(),
            vec![0, 1, 2]
        );

        // Test empty string
        assert_eq!(parse_cpu_range_list("").unwrap(), Vec::<usize>::new());

        // Test empty parts
        assert_eq!(parse_cpu_range_list("0,,2").unwrap(), vec![0, 2]);

        // Test single value range
        assert_eq!(parse_cpu_range_list("3-3").unwrap(), vec![3]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_parse_cpu_range_list_errors() {
        // Test invalid numbers
        assert!(parse_cpu_range_list("abc").is_err());
        assert!(parse_cpu_range_list("0-abc").is_err());
        assert!(parse_cpu_range_list("abc-5").is_err());

        // Test malformed ranges
        assert!(parse_cpu_range_list("-5").is_err());
        assert!(parse_cpu_range_list("5-").is_err());
        assert!(parse_cpu_range_list("--").is_err());
    }

    #[test]
    fn test_cpu_count() {
        // cpu_count should return max_cpu_id + 1
        match cpu_count() {
            Ok(count) => {
                assert!(count > 0, "CPU count should be at least 1");
            }
            Err(CpuAffinityError::NotSupported) => {
                // Expected on non-Linux platforms
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_max_cpu_id_reasonable() {
        match max_cpu_id() {
            Ok(max) => {
                // Most systems have < 1024 CPUs
                assert!(max < CPU_SETSIZE, "max_cpu_id should be less than CPU_SETSIZE");
                // max is usize, so it's always >= 0
            }
            Err(e) => panic!("Failed to get max_cpu_id: {:?}", e),
        }
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_not_supported_on_non_linux() {
        assert_eq!(set_cpu_affinity([0]).unwrap_err(), CpuAffinityError::NotSupported);
        assert_eq!(cpu_affinity().unwrap_err(), CpuAffinityError::NotSupported);
        assert_eq!(max_cpu_id().unwrap_err(), CpuAffinityError::NotSupported);
        assert_eq!(isolated_cpus().unwrap_err(), CpuAffinityError::NotSupported);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_set_cpu_affinity_validation() {
        // Test empty list
        assert!(matches!(
            set_cpu_affinity([]).unwrap_err(),
            CpuAffinityError::EmptyCpuList
        ));

        // Test invalid CPU (way too high)
        let result = set_cpu_affinity([99999]);
        assert!(matches!(
            result.unwrap_err(),
            CpuAffinityError::InvalidCpu { .. }
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cpu_affinity_returns_sorted() {
        // Get current affinity - should be sorted
        if let Ok(cpus) = cpu_affinity() {
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "cpu_affinity should return sorted CPU list");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_isolated_cpus_returns_sorted() {
        // isolated_cpus should return a sorted list
        if let Ok(cpus) = isolated_cpus() {
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "isolated_cpus should return sorted CPU list");
        }
    }
}
