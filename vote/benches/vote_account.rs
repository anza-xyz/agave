use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::Rng,
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{VoteInit, VoteStateV4, VoteStateVersions},
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
        commission: rng.gen(),
    };
    let clock = solana_clock::Clock {
        slot: rng.gen(),
        epoch_start_timestamp: rng.gen(),
        epoch: rng.gen(),
        leader_schedule_epoch: rng.gen(),
        unix_timestamp: rng.gen(),
    };
    let vote_state = VoteStateV4::new(&vote_pubkey, &vote_init, &clock);
    let account = AccountSharedData::new_data(
        rng.gen(), // lamports
        &VoteStateVersions::new_v4(vote_state.clone()),
        &solana_sdk_ids::vote::id(), // owner
    )
    .unwrap();
    (account, vote_state)
}

fn bench_vote_account_try_from(b: &mut Bencher) {
    let mut rng = rand::thread_rng();
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

benchmark_group!(benches, bench_vote_account_try_from);
benchmark_main!(benches);
