use {
    criterion::{Criterion, criterion_group, criterion_main},
    rand::{Rng, rng},
    solana_accounts_db::{
        account_info::AccountInfo,
        accounts_index::{
            ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS, AccountsIndex, ReclaimsSlotList, UpsertReclaim,
        },
    },
    std::sync::Arc,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn bench_accounts_index(c: &mut Criterion) {
    const NUM_PUBKEYS: usize = 10_000;
    let pubkeys: Vec<_> = (0..NUM_PUBKEYS)
        .map(|_| solana_pubkey::new_rand())
        .collect();

    const NUM_FORKS: u64 = 16;

    let mut reclaims = ReclaimsSlotList::new();
    let index = AccountsIndex::<AccountInfo, AccountInfo>::new(
        &ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS,
        Arc::default(),
    );
    for f in 0..NUM_FORKS {
        for pubkey in pubkeys.iter().take(NUM_PUBKEYS) {
            index.upsert(
                f,
                f,
                pubkey,
                AccountInfo::default(),
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
        }
    }

    let mut fork = NUM_FORKS;
    let mut root = 0;
    c.bench_function("accounts_index", |b| {
        b.iter(|| {
            for _p in 0..NUM_PUBKEYS {
                let pubkey = rng().random_range(0..NUM_PUBKEYS);
                index.upsert(
                    fork,
                    fork,
                    &pubkeys[pubkey],
                    AccountInfo::default(),
                    &mut reclaims,
                    UpsertReclaim::PopulateReclaims,
                );
                reclaims.clear();
            }
            index.add_root(root);
            root = root.checked_add(1).expect("root overflow");
            fork = fork.checked_add(1).expect("fork overflow");
        });
    });
}

criterion_group!(benches, bench_accounts_index);
criterion_main!(benches);
