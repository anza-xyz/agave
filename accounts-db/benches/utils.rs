use {
    rand::{
        distributions::{Distribution, WeightedIndex},
        Rng, SeedableRng,
    },
    rand_chacha::ChaChaRng,
    solana_sdk::{
        account::AccountSharedData, pubkey::Pubkey, rent::Rent,
        rent_collector::RENT_EXEMPT_RENT_EPOCH,
    },
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
        let owner: [u8; 32] = rng.gen();
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
#[allow(dead_code)]
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

    iter::from_fn(move || {
        let index = distribution.sample(&mut rng);
        let data_size = data_sizes[index];
        sum = sum.saturating_add(data_size);
        if sum >= size_limit {
            None
        } else {
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
