//! Tests for vote_account that require BLS signatures.
//! These tests are in the runtime crate to avoid miri cross-compilation issues
//! with the blst crate in the vote crate.

use {
    rand::Rng,
    solana_account::{AccountSharedData, WritableAccount},
    solana_bls_signatures::{
        keypair::Keypair as BLSKeypair, pubkey::PubkeyCompressed as BLSPubkeyCompressed,
    },
    solana_pubkey::Pubkey,
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::{VoteInit, VoteStateV4, VoteStateVersions},
    },
};

const MIN_STAKE_FOR_STAKED_ACCOUNT: u64 = 1;
const MAX_STAKE_FOR_STAKED_ACCOUNT: u64 = 997;

/// Creates a vote account
/// `set_bls_pubkey`: controls whether the bls pubkey is None or Some
fn new_rand_vote_account<R: Rng>(
    rng: &mut R,
    node_pubkey: Option<Pubkey>,
    set_bls_pubkey: bool,
) -> AccountSharedData {
    let vote_init = VoteInit {
        node_pubkey: node_pubkey.unwrap_or_else(Pubkey::new_unique),
        authorized_voter: Pubkey::new_unique(),
        authorized_withdrawer: Pubkey::new_unique(),
        commission: rng.random(),
    };
    let bls_pubkey_compressed = if set_bls_pubkey {
        let bls_pubkey: BLSPubkeyCompressed = BLSKeypair::new().public.into();
        let bls_pubkey_buffer = bincode::serialize(&bls_pubkey).unwrap();
        Some(bls_pubkey_buffer.try_into().unwrap())
    } else {
        None
    };
    let vote_state = VoteStateV4 {
        node_pubkey: vote_init.node_pubkey,
        authorized_voters: AuthorizedVoters::new(0, vote_init.authorized_voter),
        authorized_withdrawer: vote_init.authorized_withdrawer,
        bls_pubkey_compressed,
        ..VoteStateV4::default()
    };

    AccountSharedData::new_data(
        rng.random(), // lamports
        &VoteStateVersions::new_v4(vote_state),
        &solana_sdk_ids::vote::id(), // owner
    )
    .unwrap()
}

/// Creates `num_nodes` random vote accounts with the specified stake
/// The first `num_nodes_with_bls_pubkeys` have the bls_pubkeys set while the rest are unset
fn new_staked_vote_accounts<R: Rng, F>(
    rng: &mut R,
    num_nodes: usize,
    num_nodes_with_bls_pubkeys: usize,
    stake_per_node: Option<u64>,
    lamports_per_node: F,
) -> VoteAccounts
where
    F: Fn(usize) -> u64,
{
    let mut vote_accounts = VoteAccounts::default();
    for index in 0..num_nodes {
        let pubkey = Pubkey::new_unique();
        let stake = stake_per_node.unwrap_or_else(|| {
            rng.random_range(MIN_STAKE_FOR_STAKED_ACCOUNT..MAX_STAKE_FOR_STAKED_ACCOUNT)
        });
        let node_pubkey = Pubkey::new_unique();
        let set_bls_pubkey = index < num_nodes_with_bls_pubkeys;
        let mut account = new_rand_vote_account(rng, Some(node_pubkey), set_bls_pubkey);
        account.set_lamports(lamports_per_node(index));
        vote_accounts.insert(pubkey, VoteAccount::try_from(account).unwrap(), || stake);
    }
    vote_accounts
}

#[test]
fn test_clone_and_filter_for_vat_truncates() {
    let mut rng = rand::rng();
    let current_limit = 3000;
    let vote_accounts =
        new_staked_vote_accounts(&mut rng, current_limit, current_limit, None, |_| {
            10_000_000_000
        });
    // All vote accounts should be returned if the limit is high enough.
    let filtered =
        vote_accounts.clone_and_filter_for_vat(current_limit + 500, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), vote_accounts.len());

    // If the limit is smaller than number of accounts, truncate it.
    let lower_limit = current_limit - 1000;
    let filtered =
        vote_accounts.clone_and_filter_for_vat(lower_limit, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert!(filtered.len() <= lower_limit);
    // Check that the filtered accounts are the same as the original accounts.
    for (pubkey, (_, vote_account)) in filtered.as_ref().iter() {
        assert_eq!(vote_accounts.get(pubkey), Some(vote_account));
    }
    // Check that the stake in any filtered account is higher than truncated accounts.
    let min_stake = filtered
        .as_ref()
        .iter()
        .map(|(_, (stake, _))| *stake)
        .min()
        .unwrap();
    for (pubkey, (stake, _vote_account)) in vote_accounts.as_ref().iter() {
        if *stake < min_stake {
            assert!(filtered.get(pubkey).is_none());
        }
    }
}

#[test]
fn test_clone_and_filter_for_vat_filters_non_alpenglow() {
    let mut rng = rand::rng();
    let num_alpenglow_nodes = 2000;
    // Check that non-alpenglow accounts are kicked out, 2000 accounts with bls pubkey, 1000 accounts without.
    let num_nodes = num_alpenglow_nodes + 1000;
    let vote_accounts =
        new_staked_vote_accounts(&mut rng, num_nodes, num_alpenglow_nodes, None, |_| {
            10_000_000_000
        });
    let new_limit = num_alpenglow_nodes + 500;
    let filtered = vote_accounts.clone_and_filter_for_vat(new_limit, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), num_alpenglow_nodes);
    // Check that all filtered accounts have bls pubkey
    for (_pubkey, (_stake, vote_account)) in filtered.as_ref().iter() {
        assert!(vote_account
            .vote_state_view()
            .bls_pubkey_compressed()
            .is_some());
    }
    // Now get only 1500 accounts, even some alpenglow accounts are kicked out
    let new_limit = num_alpenglow_nodes - 500;
    let filtered = vote_accounts.clone_and_filter_for_vat(new_limit, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert!(filtered.len() <= new_limit);
    for (_pubkey, (_stake, vote_account)) in filtered.as_ref().iter() {
        assert!(vote_account
            .vote_state_view()
            .bls_pubkey_compressed()
            .is_some());
    }
}

#[test]
fn test_clone_and_filter_for_vat_same_stake_at_border() {
    let mut rng = rand::rng();
    let num_alpenglow_nodes = 2000;
    // Create exactly num_alpenglow_nodes + 2 accounts with the same stake
    let num_accounts = num_alpenglow_nodes + 2;
    let accounts = (0..num_accounts).map(|index| {
        let mut account = new_rand_vote_account(&mut rng, None, true);
        account.set_lamports(10_000_000_000);
        let vote_account = VoteAccount::try_from(account).unwrap();
        let stake = if index < num_alpenglow_nodes - 10 {
            100 + index as u64
        } else {
            10 // same stake for the last 12 accounts
        };
        (Pubkey::new_unique(), (stake, vote_account))
    });
    let mut vote_accounts = VoteAccounts::default();
    for (pubkey, (stake, vote_account)) in accounts {
        vote_accounts.insert(pubkey, vote_account, || stake);
    }
    let filtered =
        vote_accounts.clone_and_filter_for_vat(num_accounts, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), num_accounts);
    let filtered =
        vote_accounts.clone_and_filter_for_vat(num_alpenglow_nodes, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), num_alpenglow_nodes - 10);
}

#[test]
fn test_clone_and_filter_for_vat_not_enough_lamports() {
    let mut rng = rand::rng();
    let num_alpenglow_nodes = 2000;
    let minimum_vote_account_balance = 1_600_000_000;
    // for 10% in vote accounts, set the balance below the minimum
    let entries_to_modify = num_alpenglow_nodes / 10;
    let vote_accounts = new_staked_vote_accounts(
        &mut rng,
        num_alpenglow_nodes,
        num_alpenglow_nodes,
        None,
        |index| {
            if index < entries_to_modify {
                minimum_vote_account_balance - 1
            } else {
                10_000_000_000
            }
        },
    );
    let filtered =
        vote_accounts.clone_and_filter_for_vat(num_alpenglow_nodes, minimum_vote_account_balance);
    assert!(filtered.len() <= num_alpenglow_nodes - entries_to_modify);
}

#[test]
#[should_panic(expected = "no valid vote accounts found")]
fn test_clone_and_filter_for_vat_empty_accounts() {
    let mut rng = rand::rng();
    let current_limit = 3000;
    let vote_accounts = new_staked_vote_accounts(
        &mut rng,
        current_limit,
        current_limit,
        Some(100), // Set all vote accounts to equal stake of 100
        |_| 10_000_000_000,
    );
    // Since everyone has same stake, and we set the limit to 500 less than the number of accounts,
    // we will end up with no accounts after filtering, because all accounts with the same stake
    // at the border will be removed. This should panic.
    let _filtered =
        vote_accounts.clone_and_filter_for_vat(current_limit - 500, MIN_STAKE_FOR_STAKED_ACCOUNT);
}
