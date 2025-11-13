//! CPU Spinner with PoH Speed Check
//!
//! This example pins to a specific CPU core and runs PoH (Proof of History)
//! hash calculations continuously, measuring performance.
//!
//! Usage: cargo run --example cpu_spinner -- <cpu_id> <timeout_secs>
//!
//! Example:
//!   cargo run --example cpu_spinner -- 0 10  # Pin to CPU 0, run for 10 seconds
//!   cargo run --example cpu_spinner -- 2 30  # Pin to CPU 2, run for 30 seconds

use agave_cpu_utils::*;
use sha2::{Digest, Sha256};
use std::env;
use std::time::{Duration, Instant};

/// SHA256 hash type (32 bytes)
type Hash = [u8; 32];

/// Compute SHA256 hash of input bytes
fn hash(data: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Run PoH speed check for a given number of hash iterations
fn compute_hash_time(hashes_sample_size: u64) -> (Duration, Hash) {
    let mut v = [0u8; 32]; // Start with zero hash
    let start = Instant::now();

    for _ in 0..hashes_sample_size {
        v = hash(&v);
    }

    (start.elapsed(), v)
}

/// Statistics tracking for PoH speed
struct PohStats {
    total_hashes: u64,
    total_time: Duration,
    samples: Vec<f64>,
    min_hps: f64,
    max_hps: f64,
}

impl PohStats {
    fn new() -> Self {
        Self {
            total_hashes: 0,
            total_time: Duration::ZERO,
            samples: Vec::new(),
            min_hps: f64::MAX,
            max_hps: 0.0,
        }
    }

    fn add_sample(&mut self, hashes: u64, duration: Duration) {
        self.total_hashes += hashes;
        self.total_time += duration;

        let hashes_per_second = hashes as f64 / duration.as_secs_f64();
        self.samples.push(hashes_per_second);

        if hashes_per_second < self.min_hps {
            self.min_hps = hashes_per_second;
        }
        if hashes_per_second > self.max_hps {
            self.max_hps = hashes_per_second;
        }
    }

    fn avg_hashes_per_second(&self) -> f64 {
        self.total_hashes as f64 / self.total_time.as_secs_f64()
    }

    fn median_hashes_per_second(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }

        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }

    fn stddev(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }

        let mean = self.samples.iter().sum::<f64>() / self.samples.len() as f64;
        let variance = self.samples.iter()
            .map(|x| {
                let diff = x - mean;
                diff * diff
            })
            .sum::<f64>() / self.samples.len() as f64;

        variance.sqrt()
    }

    fn print_stats(&self) {
        println!("\n=== PoH Speed Statistics ===");
        println!("Total hashes computed:    {}", self.total_hashes);
        println!("Total time:               {:?}", self.total_time);
        println!("Samples collected:        {}", self.samples.len());
        println!();
        println!("Average hashes/second:    {:.0}", self.avg_hashes_per_second());
        println!("Median hashes/second:     {:.0}", self.median_hashes_per_second());
        println!("Min hashes/second:        {:.0}", self.min_hps);
        println!("Max hashes/second:        {:.0}", self.max_hps);
        println!("Standard deviation:       {:.0}", self.stddev());
        println!();

        // Convert to millions of hashes per second for readability
        let avg_mhps = self.avg_hashes_per_second() / 1_000_000.0;
        let median_mhps = self.median_hashes_per_second() / 1_000_000.0;
        println!("Performance:              {:.2} MH/s (avg), {:.2} MH/s (median)", avg_mhps, median_mhps);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: {} <cpu_id> <timeout_seconds>", args[0]);
        eprintln!();
        eprintln!("Example:");
        eprintln!("  {} 0 10   # Pin to CPU 0, run for 10 seconds", args[0]);
        eprintln!("  {} 2 30   # Pin to CPU 2, run for 30 seconds", args[0]);
        std::process::exit(1);
    }

    let cpu_id: usize = args[1].parse()?;
    let timeout_secs: u64 = args[2].parse()?;
    let timeout = Duration::from_secs(timeout_secs);

    println!("=== CPU Spinner with PoH Speed Check ===");
    println!();

    // Get system info
    let cpu_count = cpu_count()?;
    let physical_cores = physical_core_count()?;

    println!("System Information:");
    println!("  Total CPUs:        {}", cpu_count);
    println!("  Physical cores:    {}", physical_cores);

    // Validate CPU ID
    if cpu_id >= cpu_count {
        eprintln!("Error: CPU {} does not exist (max CPU is {})", cpu_id, cpu_count - 1);
        std::process::exit(1);
    }

    // Check if CPU is isolated (better for benchmarking)
    let isolated = isolated_cpus()?;
    if isolated.contains(&cpu_id) {
        println!("  CPU {} status:     ISOLATED (excellent for benchmarking)", cpu_id);
    } else {
        println!("  CPU {} status:     NORMAL (may have interference)", cpu_id);
        if !isolated.is_empty() {
            println!("  Tip: Consider using one of the isolated CPUs: {:?}", isolated);
        }
    }

    // Pin to specified CPU
    println!("\nPinning to CPU {}...", cpu_id);
    set_cpu_affinity([cpu_id])?;

    // Verify affinity
    let affinity = cpu_affinity()?;
    if affinity != vec![cpu_id] {
        eprintln!("Warning: Failed to pin exclusively to CPU {} (got {:?})", cpu_id, affinity);
    } else {
        println!("Successfully pinned to CPU {}", cpu_id);
    }

    // Get current CPU (Linux-specific)
    #[cfg(target_os = "linux")]
    {
        let current = unsafe { libc::sched_getcpu() };
        if current >= 0 {
            println!("Currently executing on CPU: {}", current);
        }
    }

    // Configuration
    const HASHES_PER_SAMPLE: u64 = 1_000_000;  // 1M hashes per sample
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);  // Sample every 100ms

    println!("\n=== Running PoH Speed Check ===");
    println!("Configuration:");
    println!("  CPU ID:            {}", cpu_id);
    println!("  Duration:          {} seconds", timeout_secs);
    println!("  Hashes/sample:     {}", HASHES_PER_SAMPLE);
    println!("  Sample interval:   {:?}", SAMPLE_INTERVAL);
    println!("\nRunning...");

    let mut stats = PohStats::new();
    let overall_start = Instant::now();
    let mut last_print = Instant::now();
    let mut sample_count = 0;

    // Run until timeout
    while overall_start.elapsed() < timeout {
        let (duration, _final_hash) = compute_hash_time(HASHES_PER_SAMPLE);
        stats.add_sample(HASHES_PER_SAMPLE, duration);
        sample_count += 1;

        // Print progress every second
        if last_print.elapsed() >= Duration::from_secs(1) {
            let elapsed = overall_start.elapsed().as_secs();
            let remaining = timeout_secs.saturating_sub(elapsed);
            let current_hps = HASHES_PER_SAMPLE as f64 / duration.as_secs_f64();
            let current_mhps = current_hps / 1_000_000.0;

            print!("\r[{:3}/{:3}s] Sample #{:4}: {:.2} MH/s | Avg: {:.2} MH/s | Remaining: {:2}s    ",
                   elapsed, timeout_secs, sample_count, current_mhps,
                   stats.avg_hashes_per_second() / 1_000_000.0, remaining);

            // Force flush to update the line
            use std::io::{self, Write};
            io::stdout().flush().unwrap();

            last_print = Instant::now();
        }

        // Small delay between samples to prevent overheating
        std::thread::sleep(Duration::from_millis(10));
    }

    println!("\n\nTest completed!");

    // Print final statistics
    stats.print_stats();

    // Comparison with Solana target (approximate)
    let solana_target_hps = 2_000_000.0;  // ~2M hashes/sec for 400ms slots with 800K hashes
    let performance_ratio = stats.avg_hashes_per_second() / solana_target_hps * 100.0;

    println!("=== Performance Analysis ===");
    println!("Solana target:            ~{:.0} hashes/second", solana_target_hps);
    println!("Your performance:         {:.0}% of target", performance_ratio);

    if performance_ratio >= 150.0 {
        println!("Result:                   EXCELLENT - Well above requirements");
    } else if performance_ratio >= 100.0 {
        println!("Result:                   GOOD - Meets requirements");
    } else if performance_ratio >= 75.0 {
        println!("Result:                   MARGINAL - May work with optimizations");
    } else {
        println!("Result:                   INSUFFICIENT - CPU too slow for validator");
    }

    Ok(())
}