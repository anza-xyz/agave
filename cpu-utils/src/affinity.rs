//! Core CPU affinity operations.

use crate::error::CpuAffinityError;
#[cfg(target_os = "linux")]
use std::{collections::BTreeSet, fs, io};

/// Maximum valid index into `libc::cpu_set_t`. Compile-time constant (1023).
#[cfg(target_os = "linux")]
const MAX_CPU_INDEX: usize = libc::CPU_SETSIZE as usize - 1;

/// Set CPU affinity for a thread.
///
/// Restricts the thread to run only on the specified CPUs. Duplicate CPU IDs are
/// automatically deduplicated.
///
/// # Arguments
/// * `pid` - Thread ID to set affinity for. `None` means the calling thread.
/// * `cpus` - CPU IDs to bind the thread to. Can be any iterable collection.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// // Pin current thread to CPU 0
/// set_cpu_affinity(None, [0])?;
///
/// // Pin current thread to multiple CPUs
/// set_cpu_affinity(None, [0, 1, 2])?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::EmptyCpuList`] if the CPU list is empty.
/// Returns [`CpuAffinityError::InvalidCpu`] if any CPU ID exceeds the system maximum
/// or the `cpu_set_t` capacity (`CPU_SETSIZE`).
/// Returns [`CpuAffinityError::Io`] if the system call fails (e.g., permission denied).
/// Returns [`CpuAffinityError::ParseError`] if sysfs CPU data is malformed.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
///
#[cfg(target_os = "linux")]
pub fn set_cpu_affinity(
    pid: Option<libc::pid_t>,
    cpus: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // Use the tighter bound: max online CPU, clamped to CPU_SETSIZE to prevent
    // out-of-bounds access in CPU_SET/CPU_ISSET.
    let max_cpu = max_cpu_id()?.min(MAX_CPU_INDEX);

    // validate, deduplicate via CPU_ISSET, and set CPUs
    for cpu in cpus {
        if cpu > max_cpu {
            return Err(CpuAffinityError::InvalidCpu { cpu, max: max_cpu });
        }

        // safety: CPU_ISSET is safe after validation above
        if unsafe { libc::CPU_ISSET(cpu, &cpu_set) } {
            continue;
        }

        // safety: We've validated cpu is within valid range
        unsafe {
            libc::CPU_SET(cpu, &mut cpu_set);
        }
    }

    // safety: CPU_COUNT is safe with a valid cpu_set_t
    if unsafe { libc::CPU_COUNT(&cpu_set) } == 0 {
        return Err(CpuAffinityError::EmptyCpuList);
    }

    let tid = pid.unwrap_or(0);

    // safety: sched_setaffinity is safe with valid parameters
    let result =
        unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &cpu_set) };

    if result != 0 {
        return Err(CpuAffinityError::Io(io::Error::last_os_error()));
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_cpu_affinity(
    _pid: Option<i32>,
    _cpus: impl IntoIterator<Item = usize>,
) -> Result<(), CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the CPU affinity mask for a thread.
///
/// Returns a sorted vector of CPU IDs that the thread is allowed to run on.
///
/// # Arguments
/// * `pid` - Thread ID to query. `None` means the calling thread.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let cpus = cpu_affinity(None)?;
/// println!("Thread can run on CPUs: {:?}", cpus);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::Io`] if the system call fails.
/// Returns [`CpuAffinityError::ParseError`] if sysfs CPU data is malformed.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn cpu_affinity(pid: Option<libc::pid_t>) -> Result<Vec<usize>, CpuAffinityError> {
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    let tid = pid.unwrap_or(0);

    // safety: sched_getaffinity is safe with valid parameters
    let result = unsafe {
        libc::sched_getaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &mut cpu_set)
    };

    if result != 0 {
        return Err(CpuAffinityError::Io(io::Error::last_os_error()));
    }

    let max_cpu = max_cpu_id()?;
    let mut cpus = Vec::new();

    for cpu in 0..=max_cpu.min(MAX_CPU_INDEX) {
        // safety: CPU_ISSET is safe with cpu < CPU_SETSIZE
        if unsafe { libc::CPU_ISSET(cpu, &cpu_set) } {
            cpus.push(cpu);
        }
    }

    Ok(cpus)
}

#[cfg(not(target_os = "linux"))]
pub fn cpu_affinity(_pid: Option<i32>) -> Result<Vec<usize>, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the maximum CPU ID on the system (online CPUs only).
///
/// Reads from `/sys/devices/system/cpu/online` and parses the range list.
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
/// Returns [`CpuAffinityError::Io`] if unable to read sysfs.
/// Returns [`CpuAffinityError::ParseError`] if the sysfs data is malformed.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn max_cpu_id() -> Result<usize, CpuAffinityError> {
    let content =
        fs::read_to_string("/sys/devices/system/cpu/online").map_err(CpuAffinityError::Io)?;
    let cpus = parse_cpu_range_list(content.trim())?;
    cpus.last()
        .copied()
        .ok_or_else(|| CpuAffinityError::ParseError("no online CPUs found".into()))
}

#[cfg(not(target_os = "linux"))]
pub fn max_cpu_id() -> Result<usize, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the total number of online CPUs on the system.
///
/// Returns the count of online logical CPUs (includes hyperthreads).
/// Note: This returns only online CPUs, not all present CPUs.
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
/// Returns [`CpuAffinityError::Io`] if unable to read sysfs.
/// Returns [`CpuAffinityError::ParseError`] if the sysfs data is malformed.
/// Returns [`CpuAffinityError::NotSupported`] on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn cpu_count() -> Result<usize, CpuAffinityError> {
    let content =
        fs::read_to_string("/sys/devices/system/cpu/online").map_err(CpuAffinityError::Io)?;
    let cpus = parse_cpu_range_list(content.trim())?;
    if cpus.is_empty() {
        return Err(CpuAffinityError::ParseError("no online CPUs found".into()));
    }
    Ok(cpus.len())
}

#[cfg(not(target_os = "linux"))]
pub fn cpu_count() -> Result<usize, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Get the list of isolated CPUs.
///
/// Isolated CPUs are those reserved via kernel boot parameters (`isolcpus=...`)
/// for low-latency or real-time workloads. The kernel scheduler avoids placing
/// regular tasks on these CPUs.
///
/// # Returns
/// A sorted vector of isolated CPU IDs, or an empty vector if none are isolated
/// or the sysfs file does not exist.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> Result<(), CpuAffinityError> {
/// let isolated = isolated_cpus()?;
/// if !isolated.is_empty() {
///     println!("Isolated CPUs: {:?}", isolated);
///     set_cpu_affinity(None, [isolated[0]])?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`CpuAffinityError::Io`] if the sysfs file exists but cannot be read.
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
            parse_cpu_range_list(content)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CpuAffinityError::Io(e)),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn isolated_cpus() -> Result<Vec<usize>, CpuAffinityError> {
    Err(CpuAffinityError::NotSupported)
}

/// Parse a CPU range list string (e.g., "0-3,5,7-9") into a sorted vector of CPU IDs.
#[cfg(target_os = "linux")]
fn parse_cpu_range_list(s: &str) -> Result<Vec<usize>, CpuAffinityError> {
    let mut cpus = BTreeSet::new();

    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some((start_str, end_str)) = part.split_once('-') {
            let start = start_str
                .trim()
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU range: {part}")))?;
            let end = end_str
                .trim()
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU range: {part}")))?;

            if start > end {
                return Err(CpuAffinityError::ParseError(format!(
                    "Invalid CPU range (start > end): {part}"
                )));
            }
            cpus.extend(start..=end);
        } else {
            let cpu = part
                .parse::<usize>()
                .map_err(|_| CpuAffinityError::ParseError(format!("Invalid CPU ID: {part}")))?;
            cpus.insert(cpu);
        }
    }

    Ok(cpus.into_iter().collect())
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
        assert_eq!(parse_cpu_range_list("0,1,0,2,1").unwrap(), vec![0, 1, 2]);

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
        assert!(parse_cpu_range_list("abc").is_err());
        assert!(parse_cpu_range_list("0-abc").is_err());
        assert!(parse_cpu_range_list("abc-5").is_err());
        assert!(parse_cpu_range_list("-5").is_err());
        assert!(parse_cpu_range_list("5-").is_err());
        assert!(parse_cpu_range_list("--").is_err());
        // Inverted range
        assert!(parse_cpu_range_list("5-3").is_err());
    }

    #[test]
    fn test_cpu_count() {
        match cpu_count() {
            Ok(count) => {
                assert!(count > 0, "CPU count should be at least 1");
            }
            Err(CpuAffinityError::NotSupported) => {
                // Expected on non-Linux platforms
            }
            Err(e) => panic!("Unexpected error: {e:?}"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_max_cpu_id_reasonable() {
        match max_cpu_id() {
            Ok(max) => {
                assert!(
                    max < libc::CPU_SETSIZE as usize,
                    "max_cpu_id should be less than CPU_SETSIZE"
                );
            }
            Err(e) => panic!("Failed to get max_cpu_id: {e:?}"),
        }
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_not_supported_on_non_linux() {
        assert!(matches!(
            set_cpu_affinity(None, [0]).unwrap_err(),
            CpuAffinityError::NotSupported
        ));
        assert!(matches!(
            cpu_affinity(None).unwrap_err(),
            CpuAffinityError::NotSupported
        ));
        assert!(matches!(
            max_cpu_id().unwrap_err(),
            CpuAffinityError::NotSupported
        ));
        assert!(matches!(
            isolated_cpus().unwrap_err(),
            CpuAffinityError::NotSupported
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_set_cpu_affinity_validation() {
        // Test empty list
        assert!(matches!(
            set_cpu_affinity(None, []).unwrap_err(),
            CpuAffinityError::EmptyCpuList
        ));

        // Test invalid CPU (way too high)
        let result = set_cpu_affinity(None, [99999]);
        assert!(matches!(
            result.unwrap_err(),
            CpuAffinityError::InvalidCpu { .. }
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cpu_affinity_returns_sorted() {
        if let Ok(cpus) = cpu_affinity(None) {
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "cpu_affinity should return sorted CPU list");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_isolated_cpus_returns_sorted() {
        if let Ok(cpus) = isolated_cpus() {
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "isolated_cpus should return sorted CPU list");
        }
    }
}
