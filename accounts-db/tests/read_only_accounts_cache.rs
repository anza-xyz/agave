use {
    rand::{rngs::SmallRng, SeedableRng},
    solana_account::{Account, AccountSharedData},
    solana_accounts_db::read_only_accounts_cache::{ReadOnlyAccountsCache, CACHE_ENTRY_SIZE},
    solana_pubkey::Pubkey,
    std::{collections::HashSet, sync::atomic::Ordering},
    test_case::test_matrix,
};

/// Checks whether the evicted items are relatively old by ensuring that
/// evictions done on the oldest half of elements don't exceed the
/// probabilistic threshold.
#[test_matrix([
    (50, 45),
    (500, 450),
    (5000, 4500),
    (50_000, 49_000)
], [8, 10, 16]
)]
fn test_read_only_accounts_cache_eviction(num_accounts: (usize, usize), evict_sample_size: usize) {
    const DATA_SIZE: usize = 19;
    let (num_accounts_hi, num_accounts_lo) = num_accounts;
    let max_cache_size = num_accounts_lo.saturating_mul(CACHE_ENTRY_SIZE.saturating_add(DATA_SIZE));
    // Use SmallRng as it's faster than the default ChaCha and we don't
    // need a crypto rng here.
    let mut rng = SmallRng::from_os_rng();
    let cache = ReadOnlyAccountsCache::new(
        max_cache_size,
        usize::MAX, // <-- do not evict in the background
        evict_sample_size,
    );
    let data = vec![0u8; DATA_SIZE];
    let mut newer_half = HashSet::new();
    for i in 0..num_accounts_hi {
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::from(Account {
            lamports: 100,
            data: data.clone(),
            executable: false,
            rent_epoch: 0,
            owner: pubkey,
        });
        let slot = 0;
        cache.store(pubkey, slot, account.clone());
        if i >= num_accounts_hi / 2 {
            // Store some of the most recently used accounts so we can
            // check that we don't evict from this set.
            newer_half.insert(pubkey);
        }
    }
    assert_eq!(cache.cache_len(), num_accounts_hi);

    let mut evicts: usize = 0;
    let mut evicts_from_newer_half: usize = 0;
    let mut evicted = vec![];
    for _ in 0..1000 {
        cache.evict_in_foreground(evict_sample_size, &mut rng, |pubkey, entry| {
            let entry = entry.unwrap();
            evicts = evicts.saturating_add(1);
            if newer_half.contains(pubkey) {
                evicts_from_newer_half = evicts_from_newer_half.saturating_add(1);
            }
            evicted.push((*pubkey, entry));
        });
        assert!(!evicted.is_empty());
        for (pubkey, entry) in evicted.drain(..) {
            cache.store_with_timestamp(
                pubkey,
                entry.slot,
                entry.account,
                entry.last_update_time.load(Ordering::Relaxed),
            );
        }
    }

    // Probability of evicting the bottom half is:
    //
    // P = 1 - (1 - (50/100))^K
    //
    // Which gives around 0.984375 (98.43%). Given this result, it's safe to
    // assume that the error margin should not exceed 3%.
    let error_margin = (evicts_from_newer_half as f64) / (evicts as f64);
    assert!(error_margin < 0.03);
}
