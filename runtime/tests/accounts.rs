use {
    log::*,
    rand::{thread_rng, Rng},
    rayon::prelude::*,
    solana_accounts_db::{
        accounts_db::{AccountsDb, LoadHint},
        accounts_hash::AccountHash,
        ancestors::Ancestors,
    },
    solana_sdk::{
        account::{AccountSharedData, WritableAccount},
        clock::Slot,
        hash::Hash,
    },
    std::{collections::HashSet, time::Instant},
};

#[test]
fn test_bad_bank_hash() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let some_slot: Slot = 0;
    let max_accounts = 200;
    let mut accounts_keys: Vec<_> = (0..max_accounts)
        .into_par_iter()
        .map(|_| {
            let key = solana_pubkey::new_rand();
            let lamports = thread_rng().gen_range(0..100);
            let some_data_len = thread_rng().gen_range(0..1000);
            let account = AccountSharedData::new(lamports, some_data_len, &key);
            (key, account)
        })
        .collect();

    let mut existing = HashSet::new();
    let mut last_print = Instant::now();
    for i in 0..5_000 {
        let some_slot = some_slot + i;
        let ancestors = Ancestors::from(vec![some_slot]);

        if last_print.elapsed().as_millis() > 5000 {
            info!("i: {}", i);
            last_print = Instant::now();
        }
        let num_accounts = thread_rng().gen_range(0..100);
        (0..num_accounts).for_each(|_| {
            let mut idx;
            loop {
                idx = thread_rng().gen_range(0..max_accounts);
                if existing.contains(&idx) {
                    continue;
                }
                existing.insert(idx);
                break;
            }
            accounts_keys[idx]
                .1
                .set_lamports(thread_rng().gen_range(0..1000));
        });

        let account_refs: Vec<_> = existing
            .iter()
            .map(|idx| (&accounts_keys[*idx].0, &accounts_keys[*idx].1))
            .collect();
        db.store_cached((some_slot, &account_refs[..]), None);
        for pass in 0..2 {
            for (key, account) in &account_refs {
                if pass == 1 {
                    assert_eq!(
                        db.load_account_hash(
                            &ancestors,
                            key,
                            Some(some_slot),
                            LoadHint::Unspecified
                        )
                        .unwrap(),
                        AccountHash(Hash::default())
                    );
                } else {
                    assert_eq!(
                        db.load_account_hash(
                            &ancestors,
                            key,
                            Some(some_slot),
                            LoadHint::Unspecified
                        )
                        .unwrap(),
                        AccountsDb::hash_account(*account, key)
                    );
                }
            }
            if pass == 0 {
                // flush the write cache so we're reading from append vecs on the next iteration
                db.add_root(some_slot);
                db.flush_accounts_cache(true, Some(some_slot));
            }
        }
        existing.clear();
    }
}
