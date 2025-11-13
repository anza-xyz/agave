//! Integration tests for agave-cpu-utils
//!
//! These tests require a Linux system and will be skipped on other platforms.
//! They test actual system interactions including CPU affinity changes.

use agave_cpu_utils::*;

#[test]
#[cfg(target_os = "linux")]
fn test_set_and_get_affinity() {
    // Save current affinity to restore later
    let original_affinity = cpu_affinity().expect("Failed to get original affinity");

    // Try to set affinity to CPU 0
    let result = set_cpu_affinity([0]);
    if result.is_ok() {
        // Verify the affinity was set
        let new_affinity = cpu_affinity().expect("Failed to get new affinity");
        assert_eq!(new_affinity, vec![0], "Affinity should be set to CPU 0");

        // Restore original affinity
        set_cpu_affinity(original_affinity.clone())
            .expect("Failed to restore original affinity");

        // Verify restoration
        let restored = cpu_affinity().expect("Failed to get restored affinity");
        assert_eq!(restored, original_affinity, "Affinity should be restored");
    } else {
        // Permission denied is acceptable in CI/containers
        match result.unwrap_err() {
            CpuAffinityError::SystemCall(msg) if msg.contains("Operation not permitted") => {
                eprintln!("Skipping affinity test: insufficient permissions");
            }
            e => panic!("Unexpected error: {:?}", e),
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_affinity_with_multiple_cpus() {
    if let Ok(cpu_count) = cpu_count() {
        if cpu_count >= 2 {
            // Save current affinity
            let original = cpu_affinity().expect("Failed to get original affinity");

            // Try to set affinity to CPUs 0 and 1
            let result = set_cpu_affinity([0, 1]);
            if result.is_ok() {
                let new_affinity = cpu_affinity().expect("Failed to get new affinity");
                assert_eq!(new_affinity, vec![0, 1], "Should be pinned to CPUs 0 and 1");

                // Restore
                set_cpu_affinity(original).expect("Failed to restore affinity");
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_physical_cores_affinity() {
    // Save current affinity
    if let Ok(original) = cpu_affinity() {
        if let Ok(physical_count) = physical_core_count() {
            if physical_count > 0 {
                // Try to set affinity to first physical core
                let result = set_affinity_physical_cores_only([0]);

                if result.is_ok() {
                    // Get the mapping to verify
                    let mapping = core_to_cpus_mapping()
                        .expect("Failed to get core mapping");

                    if let Some(core_0_cpus) = mapping.get(&0) {
                        if let Some(&first_cpu) = core_0_cpus.first() {
                            let new_affinity = cpu_affinity()
                                .expect("Failed to get new affinity");
                            assert!(
                                new_affinity.contains(&first_cpu),
                                "Should be pinned to first CPU of core 0"
                            );
                        }
                    }

                    // Restore
                    set_cpu_affinity(original).expect("Failed to restore affinity");
                }
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_isolated_cpus_format() {
    // isolated_cpus should return a sorted, deduplicated list
    match isolated_cpus() {
        Ok(cpus) => {
            // Check that the list is sorted
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            assert_eq!(cpus, sorted, "Isolated CPUs should be sorted");

            // Check for no duplicates
            let mut deduped = cpus.clone();
            deduped.dedup();
            assert_eq!(cpus, deduped, "Isolated CPUs should have no duplicates");

            // All CPU IDs should be valid
            if let Ok(max_cpu) = max_cpu_id() {
                for &cpu in &cpus {
                    assert!(
                        cpu <= max_cpu,
                        "Isolated CPU {} exceeds max CPU {}",
                        cpu,
                        max_cpu
                    );
                }
            }
        }
        Err(e) => {
            // Not having isolated CPUs is fine
            eprintln!("No isolated CPUs or error reading: {:?}", e);
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_core_mapping_completeness() {
    if let Ok(mapping) = core_to_cpus_mapping() {
        if let Ok(total_cpus) = cpu_count() {
            // Count all CPUs in the mapping
            let mapped_cpu_count: usize = mapping.values().map(|v| v.len()).sum();

            // In a healthy system, all CPUs should be mapped to cores
            // However, some CPUs might be offline, so we allow for that
            assert!(
                mapped_cpu_count <= total_cpus,
                "Mapped CPUs ({}) should not exceed total CPUs ({})",
                mapped_cpu_count,
                total_cpus
            );

            // If we have any mapping, it should be reasonable
            if !mapping.is_empty() {
                assert!(
                    mapped_cpu_count > 0,
                    "Should have at least one CPU mapped"
                );
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_cpu_count_consistency() {
    // cpu_count should equal max_cpu_id + 1
    if let (Ok(count), Ok(max_id)) = (cpu_count(), max_cpu_id()) {
        assert_eq!(
            count,
            max_id + 1,
            "CPU count should equal max_cpu_id + 1"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_physical_core_ratio() {
    if let (Ok(physical), Ok(logical)) = (physical_core_count(), cpu_count()) {
        // Physical cores should be at most equal to logical CPUs
        assert!(
            physical <= logical,
            "Physical cores ({}) should not exceed logical CPUs ({})",
            physical,
            logical
        );

        // The ratio should be reasonable (1x to 4x hyperthreading)
        if physical > 0 {
            let ratio = logical / physical;
            assert!(
                ratio >= 1 && ratio <= 4,
                "CPU to core ratio ({}) should be between 1 and 4",
                ratio
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn test_affinity_deduplication() {
    // Test that duplicate CPU IDs are handled correctly
    let original = cpu_affinity().expect("Failed to get original affinity");

    // Try to set with duplicates
    let result = set_cpu_affinity([0, 0, 0]);
    if result.is_ok() {
        let new_affinity = cpu_affinity().expect("Failed to get new affinity");
        assert_eq!(new_affinity, vec![0], "Duplicates should be deduplicated");

        // Restore
        set_cpu_affinity(original).expect("Failed to restore affinity");
    }
}

#[test]
#[cfg(not(target_os = "linux"))]
fn test_non_linux_returns_not_supported() {
    // All functions should return NotSupported on non-Linux platforms
    assert_eq!(
        set_cpu_affinity([0]).unwrap_err(),
        CpuAffinityError::NotSupported
    );
    assert_eq!(
        cpu_affinity().unwrap_err(),
        CpuAffinityError::NotSupported
    );
    assert_eq!(
        isolated_cpus().unwrap_err(),
        CpuAffinityError::NotSupported
    );
    assert_eq!(
        physical_core_count().unwrap_err(),
        CpuAffinityError::NotSupported
    );
    assert_eq!(
        core_to_cpus_mapping().unwrap_err(),
        CpuAffinityError::NotSupported
    );
    assert_eq!(
        set_affinity_physical_cores_only([0]).unwrap_err(),
        CpuAffinityError::NotSupported
    );
}