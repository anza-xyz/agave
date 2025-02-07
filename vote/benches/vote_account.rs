#![feature(test)]
extern crate test;

use {
    rand::Rng,
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{VoteInit, VoteState, VoteStateVersions},
    test::Bencher,
};

fn new_rand_vote_account<R: Rng>(
    rng: &mut R,
    node_pubkey: Option<Pubkey>,
) -> (AccountSharedData, VoteState) {
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
    let vote_state = VoteState::new(&vote_init, &clock);
    let account = AccountSharedData::new_data(
        rng.random(), // lamports
        &VoteStateVersions::new_current(vote_state.clone()),
        &solana_sdk_ids::vote::id(), // owner
    )
    .unwrap();
    (account, vote_state)
}

#[bench]
fn bench_vote_account_try_from(b: &mut Bencher) {
    let mut rng = rand::rng();
    let (account, vote_state) = new_rand_vote_account(&mut rng, None);

    b.iter(|| {
        let vote_account = VoteAccount::try_from(account.clone()).unwrap();
        let state = vote_account.vote_state();
        assert_eq!(state, &vote_state);
    });
}
