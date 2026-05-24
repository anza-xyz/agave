#![allow(clippy::arithmetic_side_effects)]
use {
    clap::{Arg, Command, crate_description, crate_name},
    solana_entry::entry::{EntrySlice, create_ticks},
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
    while num_entries <= max_num_entries as usize {
        let mut time = Measure::start("time");
        for _ in 0..iterations {
            assert!(thread_pool.install(|| {
                ticks[..num_entries]
                    .verify_cpu_generic(&start_hash)
                    .status()
            }));
        }
        time.stop();
        println!(
            "{},cpu_generic,{}",
            num_entries,
            time.as_us() / iterations as u64
        );

        println!();
        num_entries *= 2;
    }
}
