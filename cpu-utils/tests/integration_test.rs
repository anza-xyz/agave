#![cfg(all(feature = "agave-unstable-api", target_os = "linux"))]

//! Integration tests for agave-cpu-utils
//!
//! These tests require a Linux system.
//! Tests that modify CPU affinity spawn dedicated threads to avoid affecting the
//! test harness.

use {
    agave_cpu_utils::{CpuId, cpu_affinity, set_cpu_affinity},
    std::{io, thread},
};

fn current_affinity() -> Vec<CpuId> {
    cpu_affinity(None).expect("failed to query current CPU affinity")
}

fn handle_affinity_result(result: io::Result<()>, test_name: &str) {
    match result {
        Ok(()) => {}
        Err(e) => panic!("{test_name}: unexpected error: {e:?}"),
    }
}

#[test]
fn test_set_and_get_affinity() {
    let affinity = current_affinity();
    let cpu = affinity
        .first()
        .copied()
        .expect("current CPU affinity mask should not be empty");

    let result = thread::spawn(move || {
        set_cpu_affinity(None, [cpu])?;
        let affinity = cpu_affinity(None)?;
        assert_eq!(affinity, vec![cpu]);
        Ok::<(), io::Error>(())
    })
    .join()
    .expect("Thread panicked");

    handle_affinity_result(result, "test_set_and_get_affinity");
}

#[test]
fn test_affinity_with_multiple_cpus() {
    let available = current_affinity();
    if available.len() < 2 {
        eprintln!(
            "Skipping test_affinity_with_multiple_cpus: current CPU affinity mask has fewer than \
             2 CPUs"
        );
        return;
    }
    let (cpu0, cpu1) = (available[0], available[1]);

    let result = thread::spawn(move || {
        set_cpu_affinity(None, [cpu0, cpu1])?;
        let affinity = cpu_affinity(None)?;
        assert_eq!(affinity, vec![cpu0, cpu1]);
        Ok::<(), io::Error>(())
    })
    .join()
    .expect("Thread panicked");

    handle_affinity_result(result, "test_affinity_with_multiple_cpus");
}

#[test]
fn test_invalid_cpu_index_rejected() {
    assert_eq!(
        CpuId::new(libc::CPU_SETSIZE as usize)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
}
