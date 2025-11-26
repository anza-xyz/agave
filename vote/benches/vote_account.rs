use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::Rng,
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::{VoteInit, VoteStateV4, VoteStateVersions},
    std::{collections::HashMap, sync::Arc},
};

fn new_rand_vote_account<R: Rng>(
    rng: &mut R,
    node_pubkey: Option<Pubkey>,
) -> (AccountSharedData, VoteStateV4) {
    let vote_pubkey = Pubkey::new_unique();
    let vote_init = VoteInit {
        node_pubkey: node_pubkey.unwrap_or_else(Pubkey::new_unique),
        authorized_voter: Pubkey::new_unique(),
        authorized_withdrawer: Pubkey::new_unique(),
        commission: rng.random(),
    };
    let clock = solana_clock::Clock {
        slot: rng.random(),
        epoch_start_timestamp: rng.random(),
        epoch: rng.random(),
        leader_schedule_epoch: rng.random(),
        unix_timestamp: rng.random(),
    };
    let vote_state = VoteStateV4::new_with_defaults(&vote_pubkey, &vote_init, &clock);
    let account = AccountSharedData::new_data(
        rng.random(), // lamports
        &VoteStateVersions::new_v4(vote_state.clone()),
        &solana_sdk_ids::vote::id(), // owner
    )
    .unwrap();
    (account, vote_state)
}

fn bench_vote_account_try_from(b: &mut Bencher) {
    let mut rng = rand::rng();
    let (account, vote_state) = new_rand_vote_account(&mut rng, None);

    b.iter(|| {
        let vote_account = VoteAccount::try_from(account.clone()).unwrap();
        let vote_state_view = vote_account.vote_state_view();
        assert_eq!(&vote_state.node_pubkey, vote_state_view.node_pubkey());
        assert_eq!(
            vote_state.inflation_rewards_commission_bps,
            vote_state_view.inflation_rewards_commission()
        );
        assert_eq!(
            vote_state.epoch_credits.len(),
            vote_state_view.num_epoch_credits()
        );
        assert_eq!(vote_state.last_timestamp, vote_state_view.last_timestamp());
        assert_eq!(vote_state.root_slot, vote_state_view.root_slot());
    });
}

fn bench_staked_nodes_compute(b: &mut Bencher) {
    let mut rng = rand::rng();
    let mut vote_accounts_map = HashMap::new();

    // Create a scenario with ~400 vote accounts
    // Each vote account has one node_pubkey and receives delegated stake
    // The stake represents the total from multiple stake accounts delegating to it
    let num_vote_accounts = 400;

    for _ in 0..num_vote_accounts {
        let (account, _) = new_rand_vote_account(&mut rng, None);
        let vote_account = VoteAccount::try_from(account).unwrap();
        // Stake amount represents total delegated stake (from multiple stake accounts)
        let stake: u64 = rng.random_range(1_000_000..100_000_000);
        vote_accounts_map.insert(Pubkey::new_unique(), (stake, vote_account));
    }

    // Add some zero-stake accounts (vote accounts with no delegations)
    for _ in 0..50 {
        let (account, _) = new_rand_vote_account(&mut rng, None);
        let vote_account = VoteAccount::try_from(account).unwrap();
        vote_accounts_map.insert(Pubkey::new_unique(), (0, vote_account));
    }

    let vote_accounts_map = Arc::new(vote_accounts_map);

    // Benchmark measures only the staked_nodes() computation
    // Create new VoteAccounts each iteration to bypass OnceLock cache
    b.iter(|| {
        let vote_accounts = VoteAccounts::from(Arc::clone(&vote_accounts_map));
        let staked_nodes = vote_accounts.staked_nodes();
        assert!(!staked_nodes.is_empty());
    });
}

benchmark_group!(
    benches,
    bench_vote_account_try_from,
    bench_staked_nodes_compute
);
benchmark_main!(benches);
