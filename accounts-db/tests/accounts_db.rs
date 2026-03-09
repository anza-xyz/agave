use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts_db::{AccountsDb, LoadHint, PopulateReadCache},
        ancestors::Ancestors,
    },
    solana_pubkey::Pubkey,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::sleep,
        time::Duration,
    },
};

/// Regression test for the race scenario where `retry_to_get_account_accessor` would
/// incorrectly panic when the following sequence of events occurs:
///
/// 1. A load thread calls `read_index_for_accessor_or_load_slow` and gets
///    `(slot, Cached)` from the accounts index.
/// 2. A duplicate bank is detected; `remove_unrooted_slots` purges the slot (removing
///    both the accounts-index entry and the cache entry).
/// 3. The load thread's `get_account_accessor` returns `Cached(None)` because the
///    cache is now empty.
/// 4. `store_accounts_unfrozen` re-populates the slot: it writes to the cache and
///    then updates the accounts index, both with `(slot, Cached)`.
/// 5. The load thread retries `read_index_for_accessor_or_load_slow`, finds
///    `(slot, Cached)` again, and `new_slot == slot && is_store_id_equal` is true.
///
/// The fix: guard the bad-index-entry panic with `!new_storage_location.is_cached()`.
/// For the Cached variant, the sequence above is not a corruption -- the next
/// `get_account_accessor` call on the fresh `(slot, Cached)` entry will succeed.
#[test]
fn test_load_after_remove_unrooted_and_restore_to_same_slot() {
    let slot = 402240429;
    let bank_id = 1;
    let pubkey = Pubkey::new_unique();
    let account = AccountSharedData::new(42, 0, AccountSharedData::default().owner());

    let db = Arc::new(AccountsDb::new_single_for_tests());
    let ancestors = Ancestors::from(vec![(slot, 0)]);

    let exit = Arc::new(AtomicBool::new(false));

    // Control thread – performs the sequential remove-then-store cycle that mirrors
    // what happens in production when a duplicate bank is purged and the slot is
    // subsequently re-processed by the banking stage.
    let t_store = {
        let db = db.clone();
        let account = account.clone();
        let exit = exit.clone();
        std::thread::Builder::new()
            .name("control".to_string())
            .spawn(move || {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        return;
                    }
                    // Step A: purge the slot (simulate remove_unrooted_slots).
                    if db.accounts_cache.slot_cache(slot).is_some() {
                        db.remove_unrooted_slots(&[(slot, bank_id)]);
                    }
                    // Step B: re-store the account (simulate store_accounts_unfrozen).
                    db.store_for_tests((slot, &[(&pubkey, &account)][..]));
                }
            })
            .unwrap()
    };

    // Load thread – continuously attempts to load the account.
    let t_do_load = {
        let db = db.clone();
        let exit = exit.clone();
        std::thread::Builder::new()
            .name("load".to_string())
            .spawn(move || {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = db.load(
                        &ancestors,
                        &pubkey,
                        LoadHint::FixedMaxRoot,
                        PopulateReadCache::False,
                    );
                }
            })
            .unwrap()
    };

    // Prior to the fix, it failed with a panic in 'retry_to_get_account_accessor' after ~1 second,
    // run long enough to catch the failure reliably.
    sleep(Duration::from_secs(5));
    exit.store(true, Ordering::Relaxed);
    t_store.join().unwrap();
    // Propagate any panic from the load thread in retry_to_get_account_accessor).
    t_do_load.join().map_err(std::panic::resume_unwind).unwrap();
}
