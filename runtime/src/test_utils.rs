#[cfg(feature = "dev-context-only-utils")]
use {
    rand::Rng,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_bls_signatures::{
        keypair::Keypair as BLSKeypair, pubkey::PubkeyCompressed as BLSPubkeyCompressed,
    },
    solana_pubkey::Pubkey,
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::{VoteStateV4, VoteStateVersions},
    },
    std::{collections::HashMap, iter::repeat_with},
};

/// Creates a vote account
/// `set_bls_pubkey`: controls whether the bls pubkey is None or Some
#[cfg(feature = "dev-context-only-utils")]
pub fn new_rand_vote_account<F>(
    lamports: u64,
    node_pubkey: Option<Pubkey>,
    set_bls_pubkey: bool,
    pending_delegator_rewards_from_account_lamports: F,
) -> AccountSharedData
where
    F: Fn(u64) -> u64,
{
    let owner = solana_sdk_ids::vote::id();
    let mut account = AccountSharedData::new(lamports, VoteStateV4::size_of(), &owner);

    let bls_pubkey_compressed = set_bls_pubkey.then(|| {
        let bls_pubkey: BLSPubkeyCompressed = (*BLSKeypair::new().public).into();
        let bls_pubkey_buffer = bincode::serialize(&bls_pubkey).unwrap();
        bls_pubkey_buffer.try_into().unwrap()
    });
    let vote_state = VoteStateV4 {
        node_pubkey: node_pubkey.unwrap_or(Pubkey::new_unique()),
        authorized_voters: AuthorizedVoters::new(0, Pubkey::new_unique()),
        authorized_withdrawer: Pubkey::new_unique(),
        bls_pubkey_compressed,
        pending_delegator_rewards: pending_delegator_rewards_from_account_lamports(
            account.lamports(),
        ),
        ..VoteStateV4::default()
    };

    VoteStateV4::serialize(
        &VoteStateVersions::V4(Box::new(vote_state)),
        account.data_as_mut_slice(),
    )
    .unwrap();
    account
}

#[cfg(feature = "dev-context-only-utils")]
pub fn new_rand_vote_accounts<R: Rng>(
    rng: &mut R,
    num_nodes: usize,
    max_stake_for_staked_account: u64,
) -> impl Iterator<Item = (Pubkey, (/*stake:*/ u64, VoteAccount))> + '_ {
    let nodes: Vec<_> = repeat_with(Pubkey::new_unique).take(num_nodes).collect();
    repeat_with(move || {
        let node = nodes[rng.random_range(0..nodes.len())];
        let account = new_rand_vote_account(rng.random(), Some(node), true, |_| 0);
        let stake = rng.random_range(0..max_stake_for_staked_account);
        let vote_account = VoteAccount::try_from(account).unwrap();
        (Pubkey::new_unique(), (stake, vote_account))
    })
}

/// Creates `num_nodes` random vote accounts with the specified stake.
/// The first `num_nodes_with_bls_pubkeys` have the bls_pubkeys set while the rest are unset.
/// If `stake_per_node` is specified, then each node will have that stake, otherwise a random amount
/// between `min_stake_for_staked_account` and `max_stake_for_staked_account` is chosen.
#[cfg(feature = "dev-context-only-utils")]
pub fn new_staked_vote_accounts<R: Rng, F, G, H>(
    rng: &mut R,
    num_nodes: usize,
    num_nodes_with_bls_pubkeys: usize,
    stake_per_node: Option<u64>,
    min_stake_for_staked_account: u64,
    max_stake_for_staked_account: u64,
    lamports_per_node: F,
    pending_delegator_rewards_from_account_lamports_generator: G,
) -> VoteAccounts
where
    F: Fn(usize) -> u64,
    G: Fn(usize) -> H, // generator to create a function based on index
    H: Fn(u64) -> u64, // function returned by generator to set pending rewards
{
    let mut vote_accounts = VoteAccounts::default();
    for index in 0..num_nodes {
        let pubkey = Pubkey::new_unique();
        let stake = stake_per_node.unwrap_or_else(|| {
            rng.random_range(min_stake_for_staked_account..max_stake_for_staked_account)
        });
        let node_pubkey = Pubkey::new_unique();
        let set_bls_pubkey = index < num_nodes_with_bls_pubkeys;
        let pending_delegator_rewards_fn =
            pending_delegator_rewards_from_account_lamports_generator(index);
        let account = new_rand_vote_account(
            lamports_per_node(index),
            Some(node_pubkey),
            set_bls_pubkey,
            pending_delegator_rewards_fn,
        );
        vote_accounts.insert(pubkey, VoteAccount::try_from(account).unwrap(), || stake);
    }
    vote_accounts
}

#[cfg(feature = "dev-context-only-utils")]
pub fn staked_nodes<'a, I>(vote_accounts: I) -> HashMap<Pubkey, u64>
where
    I: IntoIterator<Item = &'a (Pubkey, (u64, VoteAccount))>,
{
    let mut staked_nodes = HashMap::new();
    for (_, (stake, vote_account)) in vote_accounts
        .into_iter()
        .filter(|(_, (stake, _))| *stake != 0)
    {
        staked_nodes
            .entry(*vote_account.node_pubkey())
            .and_modify(|s: &mut u64| *s = s.saturating_add(*stake))
            .or_insert(*stake);
    }
    staked_nodes
}
