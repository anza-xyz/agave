#![feature(test)]

extern crate test;

use {
    rand::{rng, Rng},
    solana_account::AccountSharedData,
    solana_accounts_db::{
        account_info::AccountInfo,
        accounts_index::{
            AccountSecondaryIndexes, AccountsIndex, ReclaimsSlotList, UpsertReclaim,
            ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS,
        },
    },
    std::sync::Arc,
    test::Bencher,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[bench]
fn bench_accounts_index(bencher: &mut Bencher) {
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
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                AccountInfo::default(),
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
        }
    }

    let mut fork = NUM_FORKS;
    let mut root = 0;
    bencher.iter(|| {
        for _p in 0..NUM_PUBKEYS {
            let pubkey = rng().random_range(0..NUM_PUBKEYS);
            index.upsert(
                fork,
                fork,
                &pubkeys[pubkey],
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                AccountInfo::default(),
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
            reclaims.clear();
        }
        index.add_root(root);
        root += 1;
        fork += 1;
    });
}
