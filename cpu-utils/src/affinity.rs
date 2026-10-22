//! Core CPU affinity operations.

use std::{io, mem, ops::Deref};

const CPU_SETSIZE: usize = libc::CPU_SETSIZE as usize;

/// Identifies a logical CPU (hardware thread) by its kernel-assigned ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CpuId(usize);

impl CpuId {
    pub fn new(cpu: usize) -> io::Result<Self> {
        if cpu < CPU_SETSIZE {
            Ok(Self(cpu))
        } else {
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        }
    }
}

impl Deref for CpuId {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Set CPU affinity for a thread.
///
/// Restricts the thread to run only on the specified CPUs.
///
/// # Arguments
/// * `thread_id` - Thread ID to set affinity for. `None` means the calling thread.
/// * `cpus` - CPU IDs to bind the thread to. Can be any iterable collection.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> std::io::Result<()> {
/// // Pin current thread to CPU 0
/// set_cpu_affinity(None, [CpuId::new(0)?])?;
///
/// // Pin current thread to multiple CPUs
/// set_cpu_affinity(None, [CpuId::new(0)?, CpuId::new(1)?, CpuId::new(2)?])?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if any CPU ID is too large for `cpu_set_t`.
/// Returns [`io::Error`] if the system call fails, including offline CPUs or CPUs
/// disallowed by the task's hard cpuset/cgroup.
pub fn set_cpu_affinity(
    thread_id: Option<libc::pid_t>,
    cpus: impl IntoIterator<Item = CpuId>,
) -> io::Result<()> {
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { mem::zeroed() };

    for cpu in cpus {
        // safety: CpuId values are constructed only after validating cpu < CPU_SETSIZE.
        unsafe { libc::CPU_SET(*cpu, &mut cpu_set) };
    }

    let tid = thread_id.unwrap_or(0);

    // safety: sched_setaffinity is safe with valid parameters
    let result =
        unsafe { libc::sched_setaffinity(tid, mem::size_of::<libc::cpu_set_t>(), &cpu_set) };

    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Get the CPU affinity mask for a thread.
///
/// Returns a sorted vector of CPU IDs that the thread is allowed to run on.
///
/// # Arguments
/// * `thread_id` - Thread ID to query. `None` means the calling thread.
///
/// # Examples
///
/// ```no_run
/// # use agave_cpu_utils::*;
/// # fn main() -> std::io::Result<()> {
/// let cpus = cpu_affinity(None)?;
/// println!("Thread can run on CPUs: {:?}", cpus);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`io::Error`] if the system call fails.
pub fn cpu_affinity(thread_id: Option<libc::pid_t>) -> io::Result<Vec<CpuId>> {
    // safety: cpu_set_t is a POD type, zero-initialization is standard
    let mut cpu_set: libc::cpu_set_t = unsafe { mem::zeroed() };

    let tid = thread_id.unwrap_or(0);

    // safety: sched_getaffinity is safe with valid parameters
    let result =
        unsafe { libc::sched_getaffinity(tid, mem::size_of::<libc::cpu_set_t>(), &mut cpu_set) };

    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cpus = Vec::new();
    for cpu in 0..CPU_SETSIZE {
        // safety: cpu < CPU_SETSIZE by construction
        if unsafe { libc::CPU_ISSET(cpu, &cpu_set) } {
            cpus.push(CpuId::new(cpu)?);
        }
    }

    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_id_validation() {
        let result = CpuId::new(CPU_SETSIZE);
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_cpu_affinity_returns_sorted() {
        let cpus = cpu_affinity(None).expect("failed to query current CPU affinity");
        assert!(
            cpus.windows(2).all(|window| *window[0] <= *window[1]),
            "cpu_affinity should return sorted CPU list"
        );
    }
}
