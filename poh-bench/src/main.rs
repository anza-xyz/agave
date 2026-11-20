#![allow(clippy::arithmetic_side_effects)]
use {
    clap::{Arg, Command, crate_description, crate_name},
    rayon::prelude::*,
    solana_entry::{
        entry::{
            EntrySlice, EntryVerificationData, create_ticks, hash_signatures, verify_entries_range,
        },
        poh::Poh,
    },
    solana_hash::Hash,
    solana_measure::measure::Measure,
    solana_sha256_hasher::hash,
    std::ops::Range,
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
                .help("Number of entries (ticks per slot with --replay)."),
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
        .arg(Arg::new("replay").long("replay").takes_value(false).help(
            "Verify a realistic slot (ticks plus transaction entries) the way replay's \
                     PoH verification jobs do",
        ))
        .arg(
            Arg::new("tx_entries_per_tick")
                .long("tx-entries-per-tick")
                .takes_value(true)
                .value_name("NUM")
                .help("--replay: average transaction entries between two ticks"),
        )
        .arg(
            Arg::new("max_txs_per_entry")
                .long("max-txs-per-entry")
                .takes_value(true)
                .value_name("NUM")
                .help("--replay: transactions per entry are uniform in 1..=NUM"),
        )
        .arg(
            Arg::new("batch_entries")
                .long("batch-entries")
                .takes_value(true)
                .value_name("NUM")
                .help("--replay: entries per batch handed to the verification workers"),
        )
        .arg(
            Arg::new("seed")
                .long("seed")
                .takes_value(true)
                .help("--replay: seed for the generated slot"),
        )
        .get_matches();

    let max_num_entries: u64 = matches.value_of_t("max_num_entries").unwrap_or(64);
    let start_num_entries: u64 = matches
        .value_of_t("start_num_entries")
        .unwrap_or(max_num_entries);
    let iterations: usize = matches.value_of_t("iterations").unwrap_or(10);
    let hashes_per_tick: u64 = matches.value_of_t("hashes_per_tick").unwrap_or(10_000);
    let start_hash = hash(&[1, 2, 3, 4]);
    let num_threads = matches.value_of_t("num_threads").unwrap_or(num_cpus::get());
    let thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("solPohBench{i:02}"))
        .build()
        .expect("new rayon threadpool");

    if matches.is_present("replay") {
        let config = ReplayConfig {
            ticks: max_num_entries,
            hashes_per_tick,
            tx_entries_per_tick: matches.value_of_t("tx_entries_per_tick").unwrap_or(10),
            max_txs_per_entry: matches.value_of_t("max_txs_per_entry").unwrap_or(8),
            batch_entries: matches.value_of_t("batch_entries").unwrap_or(64),
            seed: matches.value_of_t("seed").unwrap_or(42),
        };
        run_replay(&config, &start_hash, iterations, num_threads, &thread_pool);
        return;
    }

    let ticks = create_ticks(max_num_entries, hashes_per_tick, start_hash);
    let mut num_entries = start_num_entries as usize;
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

struct ReplayConfig {
    ticks: u64,
    hashes_per_tick: u64,
    tx_entries_per_tick: u64,
    max_txs_per_entry: u64,
    batch_entries: usize,
    seed: u64,
}

/// xorshift64*: deterministic, so every architecture verifies the same slot.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform-ish in `0..n`, `n > 0`.
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A slot of `config.ticks` ticks, each preceded by a random number (averaging
/// `tx_entries_per_tick`) of transaction entries that split the tick's
/// `hashes_per_tick` hashes at random points, as the PoH recorder does when it
/// mixes in transaction batches between ticks. Hashes are computed with `Poh`,
/// the same scalar generator a leader uses.
fn realistic_slot(config: &ReplayConfig, start_hash: &Hash) -> Vec<EntryVerificationData> {
    assert!(config.hashes_per_tick > 1 && config.max_txs_per_entry > 0);
    let mut rng = Rng(config.seed | 1);
    let mut hash = *start_hash;
    let mut entries = vec![];
    for _ in 0..config.ticks {
        let num_tx_entries = rng.below(2 * config.tx_entries_per_tick + 1);
        let mut cuts: Vec<u64> = (0..num_tx_entries)
            .map(|_| 1 + rng.below(config.hashes_per_tick - 1))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut prev_cut = 0;
        for cut in cuts {
            let num_hashes = cut - prev_cut;
            prev_cut = cut;
            let num_transactions = 1 + rng.below(config.max_txs_per_entry) as usize;
            let signatures: Vec<_> = (0..num_transactions)
                .map(|_| {
                    let mut bytes = [0u8; 64];
                    for chunk in bytes.as_chunks_mut::<8>().0 {
                        *chunk = rng.next_u64().to_le_bytes();
                    }
                    From::from(bytes)
                })
                .collect();
            let mut poh = Poh::new(hash, None);
            poh.hash(num_hashes - 1);
            hash = poh.record(hash_signatures(&signatures)).unwrap().hash;
            entries.push(EntryVerificationData {
                num_hashes,
                hash,
                num_transactions,
                signatures,
            });
        }
        let num_hashes = config.hashes_per_tick - prev_cut;
        let mut poh = Poh::new(hash, None);
        poh.hash(num_hashes - 1);
        hash = poh.tick().unwrap().hash;
        entries.push(EntryVerificationData {
            num_hashes,
            hash,
            num_transactions: 0,
            signatures: vec![],
        });
    }
    entries
}

/// The ranges replay's `send_jobs` gives the verification workers for a batch
/// of `item_count` entries: `min(workers, items)` contiguous jobs, the first
/// `item_count % job_count` of them one entry longer.
fn job_ranges(item_count: usize, num_workers: usize) -> Vec<Range<usize>> {
    let job_count = num_workers.min(item_count);
    let items_per_job = item_count / job_count;
    let remainder = item_count % job_count;
    let mut start = 0;
    (0..job_count)
        .map(|job_index| {
            let end = start + items_per_job + usize::from(job_index < remainder);
            let range = start..end;
            start = end;
            range
        })
        .collect()
}

/// Verifies a realistic slot the way replay does: split into batches of
/// `batch_entries`, each split across the workers into contiguous ranges that
/// are checked with `verify_entries_range`. Prints the verification time per
/// slot, and the total as `BENCH_RESULT` (seconds) for AutoPerf.
fn run_replay(
    config: &ReplayConfig,
    start_hash: &Hash,
    iterations: usize,
    num_workers: usize,
    thread_pool: &rayon::ThreadPool,
) {
    let entries = realistic_slot(config, start_hash);
    let mut batches = vec![];
    let mut batch_start_hash = *start_hash;
    for batch in entries.chunks(config.batch_entries.max(1)) {
        batches.push((batch_start_hash, batch));
        batch_start_hash = batch.last().unwrap().hash;
    }
    let jobs: Vec<(usize, Range<usize>)> = batches
        .iter()
        .enumerate()
        .flat_map(|(index, (_, batch))| {
            job_ranges(batch.len(), num_workers)
                .into_iter()
                .map(move |range| (index, range))
        })
        .collect();
    let num_tx_entries = entries.iter().filter(|e| e.num_transactions > 0).count();
    let num_hashes: u64 = entries.iter().map(|e| e.num_hashes).sum();
    let num_transactions: usize = entries.iter().map(|e| e.num_transactions).sum();
    println!(
        "replay: ticks={} tx_entries={num_tx_entries} transactions={num_transactions} \
         entries={} hashes={num_hashes} batches={} jobs={}",
        config.ticks,
        entries.len(),
        batches.len(),
        jobs.len(),
    );

    let mut time = Measure::start("replay");
    for _ in 0..iterations {
        assert!(thread_pool.install(|| {
            jobs.par_iter().all(|(index, range)| {
                let (batch_start_hash, batch) = &batches[*index];
                verify_entries_range(batch, range.clone(), batch_start_hash)
            })
        }));
    }
    time.stop();
    println!(
        "{},replay,{}",
        entries.len(),
        time.as_us() / iterations as u64
    );
    println!("BENCH_RESULT:{:.6}", time.as_us() as f64 / 1_000_000.0);
}
