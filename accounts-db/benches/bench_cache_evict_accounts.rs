use {
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
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

const ACCOUNT_DATA_SIZE: usize = 200;

fn bench_cache_evict_accounts(c: &mut Criterion) {
    let account_size = CACHE_ENTRY_SIZE.saturating_add(ACCOUNT_DATA_SIZE);
    let evict_account_count = AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI
        .saturating_sub(AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO)
        .div_ceil(account_size);
    let retained_account_count = AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO
        .checked_div(account_size)
        .unwrap();
    let target_data_size = retained_account_count.saturating_mul(account_size);
    let slot = 0;

    // Disable background eviction so only the timed foreground pass removes entries.
    let cache = ReadOnlyAccountsCache::new(
        target_data_size,
        usize::MAX,
        AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
        AccountsDb::DEFAULT_READ_ONLY_CACHE_NUM_SHARDS,
    );
    let data_sizes = [ACCOUNT_DATA_SIZE];
    let weights = [1];
    let mut accounts = utils::accounts(255, &data_sizes, &weights);
    for (pubkey, account) in accounts.by_ref().take(retained_account_count) {
        cache.store(pubkey, slot, account);
    }

    let mut rng = SmallRng::seed_from_u64(0);
    let mut group = c.benchmark_group("cache_evict_accounts");
    group.throughput(Throughput::Elements(
        u64::try_from(evict_account_count).unwrap(),
    ));
    group.bench_function("default", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                for (pubkey, account) in accounts.by_ref().take(evict_account_count) {
                    cache.store(pubkey, slot, account);
                }

                let start = Instant::now();
                cache.evict_in_foreground(
                    AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
                    &mut rng,
                    |_, _| {},
                );
                elapsed = elapsed.saturating_add(start.elapsed());
            }
            elapsed
        });
    });
}

criterion_group!(benches, bench_cache_evict_accounts);
criterion_main!(benches);
