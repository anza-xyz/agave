//! Integration tests for agave-cpu-utils
//!
//! These tests require a Linux system and will be skipped on other platforms.
//! Tests that modify CPU affinity spawn dedicated threads to avoid affecting the
//! test harness.

use agave_cpu_utils::*;

#[test]
#[cfg(target_os = "linux")]
fn test_set_and_get_affinity() {
    let result = std::thread::spawn(|| {
        set_cpu_affinity(None, [0])?;
        let affinity = cpu_affinity(None)?;
        assert_eq!(affinity, vec![0], "Affinity should be set to CPU 0");
        Ok::<(), CpuAffinityError>(())
    })
    .join()
    .expect("Thread panicked");

    match result {
        Ok(()) => {}
        Err(CpuAffinityError::Io(ref err))
            if err.raw_os_error() == Some(1)
                || err.to_string().contains("Operation not permitted") =>
        {
            eprintln!("Skipping affinity test: insufficient permissions");
        }
        Err(e) => panic!("Unexpected error: {e:?}"),
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_affinity_with_multiple_cpus() {
    if let Ok(count) = cpu_count() {
        if count >= 2 {
            let result = std::thread::spawn(|| {
                set_cpu_affinity(None, [0, 1])?;
                let affinity = cpu_affinity(None)?;
                assert_eq!(affinity, vec![0, 1], "Should be pinned to CPUs 0 and 1");
                Ok::<(), CpuAffinityError>(())
            })
            .join()
            .expect("Thread panicked");

            if let Err(e) = result {
                match e {
                    CpuAffinityError::Io(ref err) if err.raw_os_error() == Some(1) => {
                        eprintln!("Skipping: insufficient permissions");
                    }
                    e => panic!("Unexpected error: {e:?}"),
                }
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_physical_cores_affinity() {
    if let Ok(mapping) = core_to_cpus_mapping() {
        if let Some(&first_core) = mapping.keys().next() {
            let expected_cpu = mapping[&first_core].first().map(|c| c.0);

            let result = std::thread::spawn(move || {
                set_cpu_affinity_physical(None, [first_core])?;
                let affinity = cpu_affinity(None)?;
                if let Some(cpu) = expected_cpu {
                    assert!(
                        affinity.contains(&cpu),
                        "Should be pinned to first CPU of core {first_core:?}",
                    );
                }
                Ok::<(), CpuAffinityError>(())
            })
            .join()
            .expect("Thread panicked");

            if let Err(e) = result {
                match e {
                    CpuAffinityError::Io(ref err) if err.raw_os_error() == Some(1) => {
                        eprintln!("Skipping: insufficient permissions");
                    }
                    e => panic!("Unexpected error: {e:?}"),
                }
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_isolated_cpus_format() {
    match isolated_cpus() {
        Ok(cpus) => {
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "Isolated CPUs should be sorted");

            let mut deduped = cpus.clone();
            deduped.dedup();
            assert_eq!(cpus, deduped, "Isolated CPUs should have no duplicates");

            if let Ok(max_cpu) = max_cpu_id() {
                for &cpu in &cpus {
                    assert!(
                        cpu <= max_cpu,
                        "Isolated CPU {cpu} exceeds max CPU {max_cpu}"
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("No isolated CPUs or error reading: {e:?}");
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_core_mapping_completeness() {
    if let Ok(mapping) = core_to_cpus_mapping() {
        if let Ok(total_cpus) = cpu_count() {
            let mapped_cpu_count: usize = mapping.values().map(|v| v.len()).sum();

            assert!(
                mapped_cpu_count <= total_cpus,
                "Mapped CPUs ({mapped_cpu_count}) should not exceed total CPUs ({total_cpus})"
            );

            if !mapping.is_empty() {
                assert!(mapped_cpu_count > 0, "Should have at least one CPU mapped");
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_cpu_count_and_max_cpu_id() {
    if let (Ok(count), Ok(max_id)) = (cpu_count(), max_cpu_id()) {
        assert!(count > 0, "CPU count should be at least 1");
        assert!(
            count <= max_id + 1,
            "CPU count ({count}) should not exceed max_cpu_id + 1 ({})",
            max_id + 1
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_physical_core_ratio() {
    if let (Ok(physical), Ok(logical)) = (physical_core_count(), cpu_count()) {
        assert!(
            physical <= logical,
            "Physical cores ({physical}) should not exceed logical CPUs ({logical})"
        );

        if physical > 0 {
            let ratio = logical / physical;
            assert!(
                (1..=4).contains(&ratio),
                "CPU to core ratio ({ratio}) should be between 1 and 4"
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_affinity_deduplication() {
    let result = std::thread::spawn(|| {
        set_cpu_affinity(None, [0, 0, 0])?;
        let affinity = cpu_affinity(None)?;
        assert_eq!(affinity, vec![0], "Duplicates should be deduplicated");
        Ok::<(), CpuAffinityError>(())
    })
    .join()
    .expect("Thread panicked");

    if let Err(e) = result {
        match e {
            CpuAffinityError::Io(ref err) if err.raw_os_error() == Some(1) => {
                eprintln!("Skipping: insufficient permissions");
            }
            e => panic!("Unexpected error: {e:?}"),
        }
    }
}

#[test]
#[cfg(not(target_os = "linux"))]
fn test_non_linux_returns_not_supported() {
    assert!(matches!(
        set_cpu_affinity(None, [0]).unwrap_err(),
        CpuAffinityError::NotSupported
    ));
    assert!(matches!(
        cpu_affinity(None).unwrap_err(),
        CpuAffinityError::NotSupported
    ));
    assert!(matches!(
        isolated_cpus().unwrap_err(),
        CpuAffinityError::NotSupported
    ));
    assert!(matches!(
        physical_core_count().unwrap_err(),
        CpuAffinityError::NotSupported
    ));
    assert!(matches!(
        core_to_cpus_mapping().unwrap_err(),
        CpuAffinityError::NotSupported
    ));
    let bogus = PhysicalCpuId {
        package_id: 0,
        core_id: 0,
    };
    assert!(matches!(
        set_cpu_affinity_physical(None, [bogus]).unwrap_err(),
        CpuAffinityError::NotSupported
    ));
}
