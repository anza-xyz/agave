use {
    criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main},
    rand::{SeedableRng, rngs::SmallRng},
    solana_accounts_db::{
        accounts_db::AccountsDb,
        read_only_accounts_cache::{CACHE_ENTRY_SIZE, ReadOnlyAccountsCache},
    },
    std::time::{Duration, Instant},
};
mod utils;

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// Account data sizes to bench. 4 KiB gives the ~700k entries of mainnet-beta's
/// 3 GB cache, and 200 bytes gives ~9.6M small accounts, ~14x as many per shard.
const ACCOUNT_DATA_SIZES: &[usize] = &[200, 4096];

/// Account counts for one data size.
struct EvictSizes {
    data_size: usize,
    retained_count: usize,
    evict_count: usize,
}

/// A cache filled to its low limit, plus the seed for the next refill.
struct FilledCache {
    cache: ReadOnlyAccountsCache,
    rng: SmallRng,
    next_seed: u64,
}

impl EvictSizes {
    fn new(data_size: usize) -> Self {
        let account_size = CACHE_ENTRY_SIZE.saturating_add(data_size);
        let retained_count = AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO
            .checked_div(account_size)
            .unwrap();
        let retained_size = retained_count.saturating_mul(account_size);
        let evict_count = AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI
            .saturating_sub(retained_size)
            .div_ceil(account_size);
        Self {
            data_size,
            retained_count,
            evict_count,
        }
    }

    fn fill(&self) -> FilledCache {
        let account_size = CACHE_ENTRY_SIZE.saturating_add(self.data_size);
        let retained_size = self.retained_count.saturating_mul(account_size);

        // Disable background eviction so only the timed foreground pass removes entries.
        let cache = ReadOnlyAccountsCache::new(
            retained_size,
            usize::MAX,
            AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
            AccountsDb::DEFAULT_READ_ONLY_CACHE_NUM_SHARDS,
        );
        let data_sizes = [self.data_size];
        let weights = [1];
        for (pubkey, account) in utils::accounts(0, &data_sizes, &weights).take(self.retained_count)
        {
            cache.store(pubkey, 0, account);
        }

        FilledCache {
            cache,
            rng: SmallRng::seed_from_u64(0),
            next_seed: 1,
        }
    }

    fn evict_passes(&self, filled: &mut FilledCache, iters: u64) -> Duration {
        let mut elapsed = Duration::ZERO;
        let data_sizes = [self.data_size];
        let weights = [1];
        for _ in 0..iters {
            for (pubkey, account) in
                utils::accounts(filled.next_seed, &data_sizes, &weights).take(self.evict_count)
            {
                filled.cache.store(pubkey, 0, account);
            }
            filled.next_seed = filled.next_seed.saturating_add(1);

            let start = Instant::now();
            filled.cache.evict_in_foreground(
                AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
                &mut filled.rng,
                |_, _| {},
            );
            elapsed = elapsed.saturating_add(start.elapsed());
        }
        elapsed
    }
}

/// Benchmarks one pass of the evictor's work. The background evictor starts
/// once the cache exceeds its high size limit and stops at its low limit, so
/// each iteration stores accounts until the cache reaches the default high
/// limit, then times evicting them back down to the low limit. Each eviction
/// scans one random shard, so the result tracks accounts per shard.
fn bench_cache_evict_accounts(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_evict_accounts");
    group.sample_size(10);

    for &data_size in ACCOUNT_DATA_SIZES {
        let sizes = EvictSizes::new(data_size);
        group.throughput(Throughput::Elements(
            u64::try_from(sizes.evict_count).unwrap(),
        ));
        let mut filled = None;
        group.bench_function(BenchmarkId::from_parameter(data_size), |b| {
            let filled = filled.get_or_insert_with(|| sizes.fill());
            b.iter_custom(|iters| sizes.evict_passes(filled, iters));
        });
    }
}

criterion_group!(benches, bench_cache_evict_accounts);
criterion_main!(benches);
