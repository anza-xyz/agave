//! Side-by-side throughput comparison for the write-cache pubkey index.
//!
//! Run with:
//! `cargo bench -p solana-accounts-db --bench accounts_cache_index --features bbtree-accounts-cache-poc`.
//! Override workload size with `AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_KEYS`,
//! `AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_OPS_PER_THREAD`,
//! `AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_REPETITIONS`, and
//! `AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_THREADS` (a comma-separated list).

use {
    solana_accounts_db::{
        accounts_cache_index::{BbtreeAccountsCacheIndex, DashMapAccountsCacheIndex},
        bank_id::BankId,
    },
    solana_pubkey::Pubkey,
    std::{
        collections::BTreeSet,
        env,
        hint::black_box,
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const DEFAULT_KEYS: usize = 500_000;
const DEFAULT_OPERATIONS_PER_THREAD: usize = 250_000;
const DEFAULT_REPETITIONS: usize = 5;
const MIXED_KIND_SHIFT: u32 = 62;
const MIXED_INDEX_MASK: u64 = (1_u64 << MIXED_KIND_SHIFT) - 1;
const MIXED_READ: u64 = 0;
const MIXED_UPDATE: u64 = 1;
const MIXED_INSERT: u64 = 2;

trait BenchIndex: Default + Send + Sync + 'static {
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId);
    fn remove_batch(&self, pubkeys: &[Pubkey]) -> usize;
    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId>;
    fn collect_pubkeys(&self) -> Vec<Pubkey>;
}

impl BenchIndex for DashMapAccountsCacheIndex {
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) {
        self.insert(pubkey, bank_id);
    }

    fn remove_batch(&self, pubkeys: &[Pubkey]) -> usize {
        self.remove(pubkeys.iter().copied()).len()
    }

    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.max_bank_id_for_pubkey(pubkey)
    }

    fn collect_pubkeys(&self) -> Vec<Pubkey> {
        self.pubkeys()
    }
}

impl BenchIndex for BbtreeAccountsCacheIndex {
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) {
        self.insert(pubkey, bank_id);
    }

    fn remove_batch(&self, pubkeys: &[Pubkey]) -> usize {
        self.remove(pubkeys.iter().copied()).len()
    }

    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.max_bank_id_for_pubkey(pubkey)
    }

    fn collect_pubkeys(&self) -> Vec<Pubkey> {
        self.pubkeys()
    }
}

#[derive(Clone, Copy)]
enum Workload {
    ReadHit,
    UpdateHit,
    InsertNew,
    RemoveRetained,
    RemoveFinal,
    CollectKeys,
    Mixed,
}

impl Workload {
    const ALL: [Self; 7] = [
        Self::ReadHit,
        Self::UpdateHit,
        Self::InsertNew,
        Self::RemoveRetained,
        Self::RemoveFinal,
        Self::CollectKeys,
        Self::Mixed,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::ReadHit => "read-hit",
            Self::UpdateHit => "update-hit",
            Self::InsertNew => "insert-new",
            Self::RemoveRetained => "remove-retained",
            Self::RemoveFinal => "remove-final",
            Self::CollectKeys => "collect-keys",
            Self::Mixed => "mixed-90r-9u-1i",
        }
    }

    fn operation_count(self, threads: usize, keys: usize, operations_per_thread: usize) -> usize {
        let operations = match self {
            Self::InsertNew | Self::RemoveRetained | Self::RemoveFinal => keys,
            Self::CollectKeys => keys
                .checked_mul(threads)
                .expect("collection operation count overflow"),
            Self::ReadHit | Self::UpdateHit | Self::Mixed => operations_per_thread
                .checked_mul(threads)
                .expect("operation count overflow"),
        };
        assert!(
            operations > 0,
            "workload must execute at least one operation"
        );
        operations
    }
}

struct Config {
    keys: usize,
    operations_per_thread: usize,
    repetitions: usize,
    threads: Vec<usize>,
}

struct Inputs {
    keys: Vec<Pubkey>,
    access_indices: Vec<usize>,
    mixed_operations: Vec<u64>,
    mixed_insert_keys: Vec<Pubkey>,
}

fn main() {
    let config = Config::from_env();
    let max_threads = config
        .threads
        .iter()
        .copied()
        .max()
        .expect("thread list is nonempty");
    let inputs = Inputs::new(&config, max_threads);

    println!(
        "keys={} operations_per_thread={} repetitions={} threads={:?}",
        config.keys, config.operations_per_thread, config.repetitions, config.threads
    );
    println!("Changes within +/-5% are reported as a tie.");
    println!();
    println!("| Workload | Threads | DashMap Mops/s | bbtree Mops/s | Change | Verdict |");
    println!("|---|---:|---:|---:|---:|---|");

    for workload in Workload::ALL {
        for &threads in &config.threads {
            let mut dashmap_samples = Vec::with_capacity(config.repetitions);
            let mut bbtree_samples = Vec::with_capacity(config.repetitions);
            for repetition in 0..config.repetitions {
                if repetition % 2 == 0 {
                    dashmap_samples.push(run_sample::<DashMapAccountsCacheIndex>(
                        workload, threads, &config, &inputs,
                    ));
                    bbtree_samples.push(run_sample::<BbtreeAccountsCacheIndex>(
                        workload, threads, &config, &inputs,
                    ));
                } else {
                    bbtree_samples.push(run_sample::<BbtreeAccountsCacheIndex>(
                        workload, threads, &config, &inputs,
                    ));
                    dashmap_samples.push(run_sample::<DashMapAccountsCacheIndex>(
                        workload, threads, &config, &inputs,
                    ));
                }
            }

            let operations =
                workload.operation_count(threads, config.keys, config.operations_per_thread);
            let dashmap_mops = throughput_mops(operations, median(&mut dashmap_samples));
            let bbtree_mops = throughput_mops(operations, median(&mut bbtree_samples));
            let change = (bbtree_mops / dashmap_mops - 1.0) * 100.0;
            let verdict = if change >= 5.0 {
                "faster"
            } else if change <= -5.0 {
                "slower"
            } else {
                "tie"
            };
            println!(
                "| {} | {threads} | {dashmap_mops:.2} | {bbtree_mops:.2} | {change:+.1}% | \
                 {verdict} |",
                workload.name()
            );
        }
    }
}

impl Config {
    fn from_env() -> Self {
        let keys = env_usize("AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_KEYS", DEFAULT_KEYS);
        let operations_per_thread = env_usize(
            "AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_OPS_PER_THREAD",
            DEFAULT_OPERATIONS_PER_THREAD,
        );
        let repetitions = env_usize(
            "AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_REPETITIONS",
            DEFAULT_REPETITIONS,
        );
        let threads = env::var("AGAVE_ACCOUNTS_CACHE_INDEX_BENCH_THREADS").map_or_else(
            |_| default_thread_counts(),
            |value| parse_thread_counts(&value),
        );

        assert!(keys > 0, "benchmark needs at least one key");
        assert!(
            operations_per_thread > 0,
            "benchmark needs at least one operation"
        );
        assert!(repetitions > 0, "benchmark needs at least one repetition");
        assert!(
            !threads.is_empty(),
            "benchmark needs at least one thread count"
        );
        assert!(
            keys <= MIXED_INDEX_MASK as usize,
            "key count exceeds mixed-operation encoding"
        );

        Self {
            keys,
            operations_per_thread,
            repetitions,
            threads,
        }
    }
}

impl Inputs {
    fn new(config: &Config, max_threads: usize) -> Self {
        let keys: Vec<_> = (0..config.keys).map(pubkey_for_index).collect();
        let total_operations = max_threads
            .checked_mul(config.operations_per_thread)
            .expect("operation count overflow");
        let mut state = 0x243f_6a88_85a3_08d3;
        let mut access_indices = Vec::with_capacity(total_operations);
        let mut mixed_operations = Vec::with_capacity(total_operations);
        let mut mixed_insert_keys = Vec::new();

        for _ in 0..total_operations {
            let random = splitmix64(&mut state);
            let key_index =
                usize::try_from(random % config.keys as u64).expect("key index fits in usize");
            access_indices.push(key_index);

            let percentile = random % 100;
            let (kind, encoded_index) = if percentile == 0 {
                let insert_index = mixed_insert_keys.len();
                mixed_insert_keys.push(pubkey_for_index(
                    config
                        .keys
                        .checked_add(insert_index)
                        .expect("mixed insert key index overflow"),
                ));
                (MIXED_INSERT, insert_index)
            } else if percentile < 10 {
                (MIXED_UPDATE, key_index)
            } else {
                (MIXED_READ, key_index)
            };
            let encoded_index = u64::try_from(encoded_index).expect("mixed key index fits in u64");
            mixed_operations.push((kind << MIXED_KIND_SHIFT) | encoded_index);
        }

        Self {
            keys,
            access_indices,
            mixed_operations,
            mixed_insert_keys,
        }
    }
}

fn run_sample<I: BenchIndex>(
    workload: Workload,
    threads: usize,
    config: &Config,
    inputs: &Inputs,
) -> Duration {
    match workload {
        Workload::ReadHit => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 1));
            let operation_count = threads * config.operations_per_thread;
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let start = thread_index * config.operations_per_thread;
                    inputs.access_indices[start..start + config.operations_per_thread]
                        .iter()
                        .fold(0_u64, |checksum, &key_index| {
                            let bank_id = index
                                .max_bank_id_for_pubkey(&inputs.keys[key_index])
                                .expect("prefilled key is present");
                            checksum.wrapping_add(u64::from(bank_id))
                        })
                },
                operation_count,
            )
        }
        Workload::UpdateHit => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 1));
            let operation_count = threads * config.operations_per_thread;
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let start = thread_index * config.operations_per_thread;
                    for &key_index in
                        &inputs.access_indices[start..start + config.operations_per_thread]
                    {
                        index.insert(&inputs.keys[key_index], BankId::new(2));
                    }
                    0
                },
                operation_count,
            )
        }
        Workload::InsertNew => {
            let index = Arc::new(I::default());
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let (start, end) = chunk_bounds(inputs.keys.len(), threads, thread_index);
                    for key in &inputs.keys[start..end] {
                        index.insert(key, BankId::new(1));
                    }
                    0
                },
                inputs.keys.len(),
            )
        }
        Workload::RemoveRetained => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 2));
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let (start, end) = chunk_bounds(inputs.keys.len(), threads, thread_index);
                    index.remove_batch(&inputs.keys[start..end]) as u64
                },
                inputs.keys.len(),
            )
        }
        Workload::RemoveFinal => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 1));
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let (start, end) = chunk_bounds(inputs.keys.len(), threads, thread_index);
                    index.remove_batch(&inputs.keys[start..end]) as u64
                },
                inputs.keys.len(),
            )
        }
        Workload::CollectKeys => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 1));
            let operation_count = inputs
                .keys
                .len()
                .checked_mul(threads)
                .expect("collection operation count overflow");
            time_workers(
                index,
                threads,
                |index, _thread_index| index.collect_pubkeys().len() as u64,
                operation_count,
            )
        }
        Workload::Mixed => {
            let index = Arc::new(filled_index::<I>(&inputs.keys, 1));
            let operation_count = threads * config.operations_per_thread;
            time_workers(
                index,
                threads,
                |index, thread_index| {
                    let start = thread_index * config.operations_per_thread;
                    inputs.mixed_operations[start..start + config.operations_per_thread]
                        .iter()
                        .fold(0_u64, |checksum, &operation| {
                            let kind = operation >> MIXED_KIND_SHIFT;
                            let key_index = usize::try_from(operation & MIXED_INDEX_MASK)
                                .expect("mixed key index fits in usize");
                            match kind {
                                MIXED_READ => {
                                    let bank_id = index
                                        .max_bank_id_for_pubkey(&inputs.keys[key_index])
                                        .expect("mixed read key is present");
                                    checksum.wrapping_add(u64::from(bank_id))
                                }
                                MIXED_UPDATE => {
                                    index.insert(&inputs.keys[key_index], BankId::new(2));
                                    checksum
                                }
                                MIXED_INSERT => {
                                    index.insert(
                                        &inputs.mixed_insert_keys[key_index],
                                        BankId::new(2),
                                    );
                                    checksum
                                }
                                _ => unreachable!("invalid mixed operation kind {kind}"),
                            }
                        })
                },
                operation_count,
            )
        }
    }
}

fn filled_index<I: BenchIndex>(keys: &[Pubkey], references: usize) -> I {
    let index = I::default();
    for bank_id in 0..references {
        let bank_id = BankId::new(u64::try_from(bank_id).expect("reference count fits in u64"));
        for pubkey in keys {
            index.insert(pubkey, bank_id);
        }
    }
    index
}

fn time_workers<I, F>(index: Arc<I>, threads: usize, worker: F, operations: usize) -> Duration
where
    I: BenchIndex,
    F: Fn(&I, usize) -> u64 + Sync,
{
    let ready = Arc::new(Barrier::new(threads + 1));
    let start = Arc::new(Barrier::new(threads + 1));
    let (elapsed, checksum) = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for thread_index in 0..threads {
            let index = Arc::clone(&index);
            let ready = Arc::clone(&ready);
            let start = Arc::clone(&start);
            let worker = &worker;
            handles.push(scope.spawn(move || {
                ready.wait();
                start.wait();
                worker(&index, thread_index)
            }));
        }

        ready.wait();
        let started = Instant::now();
        start.wait();
        let checksum = handles.into_iter().fold(0_u64, |checksum, handle| {
            checksum.wrapping_add(handle.join().expect("benchmark worker panicked"))
        });
        (started.elapsed(), checksum)
    });
    black_box((&index, checksum, operations));
    elapsed
}

fn chunk_bounds(len: usize, chunks: usize, index: usize) -> (usize, usize) {
    let chunk_len = len.div_ceil(chunks);
    let start = index.saturating_mul(chunk_len).min(len);
    let end = start.saturating_add(chunk_len).min(len);
    (start, end)
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn throughput_mops(operations: usize, elapsed: Duration) -> f64 {
    operations as f64 / elapsed.as_secs_f64() / 1_000_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name).map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|error| panic!("invalid {name}={value:?}: {error}"))
    })
}

fn default_thread_counts() -> Vec<usize> {
    let available = thread::available_parallelism().map_or(1, usize::from);
    [1, 4.min(available), available]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn parse_thread_counts(value: &str) -> Vec<usize> {
    value
        .split(',')
        .map(|part| {
            let threads: usize = part
                .parse()
                .unwrap_or_else(|error| panic!("invalid thread count {part:?}: {error}"));
            assert!(threads > 0, "thread count must be positive");
            threads
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn pubkey_for_index(index: usize) -> Pubkey {
    let mut state = u64::try_from(index).expect("key index fits in u64") ^ 0x9e37_79b9_7f4a_7c15;
    let mut bytes = [0_u8; 32];
    for chunk in bytes.chunks_exact_mut(size_of::<u64>()) {
        chunk.copy_from_slice(&splitmix64(&mut state).to_le_bytes());
    }
    Pubkey::new_from_array(bytes)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
