//! Basic usage example for agave-cpu-utils
//!
//! This example demonstrates:
//! - Getting CPU and core counts
//! - Setting CPU affinity
//! - Using isolated cores
//! - Pinning to physical cores only (avoiding hyperthreads)

use agave_cpu_utils::*;

fn main() -> Result<(), CpuAffinityError> {
    println!("=== Agave CPU Utils - Basic Usage Example ===\n");

    // 1. Get system information
    println!("1. System Information:");
    let cpu_count = cpu_count()?;
    let physical_cores = physical_core_count()?;
    println!("   Total CPUs: {}", cpu_count);
    println!("   Physical cores: {}", physical_cores);

    if physical_cores < cpu_count {
        println!("   Hyperthreading: ENABLED ({}x)", cpu_count / physical_cores);
    } else {
        println!("   Hyperthreading: DISABLED");
    }

    // 2. Check current CPU affinity
    println!("\n2. Current CPU Affinity:");
    let current_affinity = cpu_affinity()?;
    println!("   Thread can run on CPUs: {:?}", current_affinity);

    // 3. Check for isolated CPUs
    println!("\n3. Isolated CPUs (via isolcpus kernel parameter):");
    let isolated = isolated_cpus()?;
    if !isolated.is_empty() {
        println!("   Found isolated CPUs: {:?}", isolated);

        // Example: Pin to first isolated CPU
        if let Some(&first_isolated) = isolated.first() {
            println!("\n   Pinning to isolated CPU {}...", first_isolated);
            set_cpu_affinity([first_isolated])?;

            let new_affinity = cpu_affinity()?;
            println!("   SUCCESS - Now pinned to CPU: {:?}", new_affinity);

            // Do some work on isolated CPU
            do_work("isolated CPU");
        }
    } else {
        println!("   No isolated CPUs found");
    }

    // 4. Get core-to-CPU mapping
    println!("\n4. Physical Core Topology:");
    let core_mapping = core_to_cpus_mapping()?;
    for (core_id, cpus) in core_mapping.iter().take(4) {
        println!("   Core {} -> CPUs {:?}", core_id, cpus);
    }
    if core_mapping.len() > 4 {
        println!("   ... and {} more cores", core_mapping.len() - 4);
    }

    // 5. Pin to physical cores only (avoid hyperthreading)
    println!("\n5. Pinning to Physical Cores Only:");
    if physical_cores >= 2 {
        println!("   Setting affinity to physical cores 0 and 1...");
        set_affinity_physical_cores_only([0, 1])?;

        let new_affinity = cpu_affinity()?;
        println!("   Now running on CPUs: {:?}", new_affinity);
        println!("   (These are the first logical CPUs of each physical core)");

        do_work("physical cores 0,1");
    }

    // 6. Pin to specific CPUs
    println!("\n6. Manual CPU Pinning:");
    if cpu_count >= 2 {
        println!("   Pinning to CPUs 0 and 1...");
        set_cpu_affinity([0, 1])?;

        let new_affinity = cpu_affinity()?;
        println!("   Now running on CPUs: {:?}", new_affinity);

        do_work("CPUs 0,1");
    }

    // 7. Best practices
    println!("\n7. Best Practices:");
    println!("   • Use isolated CPUs for critical low-latency threads (PoH, consensus)");
    println!("   • Pin to physical cores only to avoid hyperthreading interference");
    println!("   • Distribute workload across CCDs on AMD EPYC for better cache locality");
    println!("   • Monitor CPU governor (set to 'performance' for consistent latency)");

    println!("\n=== Example Complete ===");
    Ok(())
}

/// Simulate some CPU-bound work
fn do_work(context: &str) {
    println!("   Working on {}...", context);

    // Get current CPU (Linux-specific, for demonstration)
    #[cfg(target_os = "linux")]
    {
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu >= 0 {
            println!("   Actually running on CPU: {}", cpu);
        }
    }

    // Simulate work
    let start = std::time::Instant::now();
    let mut sum = 0u64;
    for i in 0..10_000_000 {
        sum = sum.wrapping_add(i);
    }

    let elapsed = start.elapsed();
    println!("   Work completed in {:?} (sum={})", elapsed, sum);
}