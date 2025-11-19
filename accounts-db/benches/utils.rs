// This file is included as a module separately in each bench, which causes
// a `dead_code` warning if the given bench doesn't `use` all functions.
#![allow(dead_code)]

use {
    rand::{
        distr::{weighted::WeightedIndex, Distribution},
        Rng, SeedableRng,
    },
    rand_chacha::ChaChaRng,
    solana_account::AccountSharedData,
    solana_accounts_db::tiered_storage::hot::RENT_EXEMPT_RENT_EPOCH,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    std::iter,
};

/// Returns an iterator with storable accounts.
pub fn accounts<'a>(
    seed: u64,
    data_sizes: &'a [usize],
    weights: &'a [usize],
) -> impl Iterator<Item = (Pubkey, AccountSharedData)> + 'a {
    let distribution = WeightedIndex::new(weights).unwrap();
    let mut rng = ChaChaRng::seed_from_u64(seed);
    let rent = Rent::default();

    iter::repeat_with(move || {
        let index = distribution.sample(&mut rng);
        let data_size = data_sizes[index];
        let owner: [u8; 32] = rng.random();
        let owner = Pubkey::new_from_array(owner);
        (
            owner,
            AccountSharedData::new_rent_epoch(
                rent.minimum_balance(data_size),
                data_size,
                &owner,
                RENT_EXEMPT_RENT_EPOCH,
            ),
        )
    })
}

/// Returns an iterator over storable accounts such that the cumulative size of
/// all accounts does not exceed the given `size_limit`.
pub fn accounts_with_size_limit<'a>(
    seed: u64,
    data_sizes: &'a [usize],
    weights: &'a [usize],
    size_limit: usize,
) -> impl Iterator<Item = (Pubkey, AccountSharedData)> + 'a {
    let distribution = WeightedIndex::new(weights).unwrap();
    let mut rng = ChaChaRng::seed_from_u64(seed);
    let rent = Rent::default();
    let mut sum = 0_usize;
    let mut stop_iter = false;

    iter::from_fn(move || {
        let index = distribution.sample(&mut rng);
        let data_size = data_sizes[index];
        sum = sum.saturating_add(data_size);
        if stop_iter {
            None
        } else {
            // If the limit is reached, include the current account as the last
            // one, then stop iterating.
            if sum >= size_limit {
                stop_iter = true;
            }
            let owner = Pubkey::new_unique();

            Some((
                owner,
                AccountSharedData::new_rent_epoch(
                    rent.minimum_balance(data_size),
                    data_size,
                    &owner,
                    RENT_EXEMPT_RENT_EPOCH,
                ),
            ))
        }
    })
}
