#![allow(clippy::arithmetic_side_effects)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use solana_entry::entry::{self, create_ticks, init_poh, EntrySlice};
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
use solana_entry::entry::{create_ticks, init_poh, EntrySlice};
use {
    clap::{crate_description, crate_name, Arg, Command},
    solana_measure::measure::Measure,
    solana_sha256_hasher::hash,
};

fn main() {
    agave_logger::setup();

    let matches = Command::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::new("max_num_entries")
                .long("max-num-entries")
                .takes_value(true)
                .value_name("SIZE")
                .help("Number of entries."),
        )
        .arg(
            Arg::new("start_num_entries")
                .long("start-num-entries")
                .takes_value(true)
                .value_name("SIZE")
                .help("Packets per chunk"),
        )
        .arg(
            Arg::new("hashes_per_tick")
                .long("hashes-per-tick")
                .takes_value(true)
                .value_name("SIZE")
                .help("hashes per tick"),
        )
        .arg(
            Arg::new("num_transactions_per_entry")
                .long("num-transactions-per-entry")
                .takes_value(true)
                .value_name("NUM")
                .help("Skip transaction sanity execution"),
        )
        .arg(
            Arg::new("iterations")
                .long("iterations")
                .takes_value(true)
                .help("Number of iterations"),
        )
        .arg(
            Arg::new("num_threads")
                .long("num-threads")
                .takes_value(true)
                .help("Number of threads"),
        )
        .get_matches();

    let max_num_entries: u64 = matches.value_of_t("max_num_entries").unwrap_or(64);
    let start_num_entries: u64 = matches
        .value_of_t("start_num_entries")
        .unwrap_or(max_num_entries);
    let iterations: usize = matches.value_of_t("iterations").unwrap_or(10);
    let hashes_per_tick: u64 = matches.value_of_t("hashes_per_tick").unwrap_or(10_000);
    let start_hash = hash(&[1, 2, 3, 4]);
    let ticks = create_ticks(max_num_entries, hashes_per_tick, start_hash);
    let mut num_entries = start_num_entries as usize;
    let num_threads = matches.value_of_t("num_threads").unwrap_or(num_cpus::get());
    let thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("solPohBench{i:02}"))
        .build()
        .expect("new rayon threadpool");
    init_poh();
    while num_entries <= max_num_entries as usize {
        let mut time = Measure::start("time");
        for _ in 0..iterations {
            assert!(ticks[..num_entries]
                .verify_cpu_generic(&start_hash, &thread_pool)
                .status());
        }
        time.stop();
        println!(
            "{},cpu_generic,{}",
            num_entries,
            time.as_us() / iterations as u64
        );

        // A target_arch check is required here since calling
        // is_x86_feature_detected from a non-x86_64 arch results in a build
        // error.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if is_x86_feature_detected!("avx2") && entry::api().is_some() {
                let mut time = Measure::start("time");
                for _ in 0..iterations {
                    assert!(ticks[..num_entries]
                        .verify_cpu_x86_simd(&start_hash, 8, &thread_pool)
                        .status());
                }
                time.stop();
                println!(
                    "{},cpu_simd_avx2,{}",
                    num_entries,
                    time.as_us() / iterations as u64
                );
            }

            if is_x86_feature_detected!("avx512f") && entry::api().is_some() {
                let mut time = Measure::start("time");
                for _ in 0..iterations {
                    assert!(ticks[..num_entries]
                        .verify_cpu_x86_simd(&start_hash, 16, &thread_pool)
                        .status())
                }
                time.stop();
                println!(
                    "{},cpu_simd_avx512,{}",
                    num_entries,
                    time.as_us() / iterations as u64
                );
            }
        }

        println!();
        num_entries *= 2;
    }
}
