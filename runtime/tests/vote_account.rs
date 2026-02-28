//! Tests for vote_account that require BLS signatures.
//! These tests are in the runtime crate to avoid miri cross-compilation issues
//! with the blst crate in the vote crate.

use {
    bincode::Options,
    rand::Rng,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_bls_signatures::{
        keypair::Keypair as BLSKeypair, pubkey::PubkeyCompressed as BLSPubkeyCompressed,
    },
    solana_pubkey::Pubkey,
    solana_runtime::bank::{MAX_ALPENGLOW_VOTE_ACCOUNTS, VAT_TO_BURN_PER_EPOCH},
    solana_vote::vote_account::{VoteAccount, VoteAccounts, VoteAccountsHashMap},
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::{VoteInit, VoteStateV4, VoteStateVersions},
    },
    std::{collections::HashMap, iter::repeat_with, sync::Arc},
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

fn new_rand_vote_accounts<R: Rng>(
    rng: &mut R,
    num_nodes: usize,
) -> impl Iterator<Item = (Pubkey, (/*stake:*/ u64, VoteAccount))> + '_ {
    let nodes: Vec<_> = repeat_with(Pubkey::new_unique).take(num_nodes).collect();
    repeat_with(move || {
        let node = nodes[rng.random_range(0..nodes.len())];
        let account = new_rand_vote_account(rng, Some(node), true);
        let stake = rng.random_range(0..MAX_STAKE_FOR_STAKED_ACCOUNT);
        let vote_account = VoteAccount::try_from(account).unwrap();
        (Pubkey::new_unique(), (stake, vote_account))
    })
}

/// Creates `num_nodes` random vote accounts with the specified stake.
/// The first `num_nodes_with_bls_pubkeys` have the bls_pubkeys set while the rest are unset.
/// If `stake_per_node` is specified, then each node will have that stake, otherwise a random amount
/// between `MIN_STAKE_FOR_STAKED_ACCOUNT` and `MAX_STAKE_FOR_STAKED_ACCOUNT` is chosen.
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

fn staked_nodes<'a, I>(vote_accounts: I) -> HashMap<Pubkey, u64>
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

#[test]
fn test_vote_account_try_from() {
    let mut rng = rand::rng();
    let account = new_rand_vote_account(&mut rng, None, true);
    let lamports = account.lamports();
    let vote_account = VoteAccount::try_from(account.clone()).unwrap();
    assert_eq!(lamports, vote_account.lamports());
    assert_eq!(&account, vote_account.account());
}

#[test]
#[should_panic(expected = "InvalidOwner")]
fn test_vote_account_try_from_invalid_owner() {
    let mut rng = rand::rng();
    let mut account = new_rand_vote_account(&mut rng, None, true);
    account.set_owner(Pubkey::new_unique());
    VoteAccount::try_from(account).unwrap();
}

#[test]
fn test_vote_account_serialize() {
    let mut rng = rand::rng();
    let account = new_rand_vote_account(&mut rng, None, true);
    let vote_account = VoteAccount::try_from(account.clone()).unwrap();
    // Assert that VoteAccount has the same wire format as Account.
    assert_eq!(
        bincode::serialize(&account).unwrap(),
        bincode::serialize(&vote_account).unwrap()
    );
}

#[test]
fn test_vote_accounts_serialize() {
    let mut rng = rand::rng();
    let vote_accounts_hash_map: VoteAccountsHashMap =
        new_rand_vote_accounts(&mut rng, 64).take(1024).collect();
    let vote_accounts = VoteAccounts::from(Arc::new(vote_accounts_hash_map.clone()));
    assert!(vote_accounts.staked_nodes().len() > 32);
    assert_eq!(
        bincode::serialize(&vote_accounts).unwrap(),
        bincode::serialize(&vote_accounts_hash_map).unwrap(),
    );
    assert_eq!(
        bincode::options().serialize(&vote_accounts).unwrap(),
        bincode::options()
            .serialize(&vote_accounts_hash_map)
            .unwrap(),
    )
}

#[test]
fn test_vote_accounts_deserialize() {
    let mut rng = rand::rng();
    let vote_accounts_hash_map: VoteAccountsHashMap =
        new_rand_vote_accounts(&mut rng, 64).take(1024).collect();
    let data = bincode::serialize(&vote_accounts_hash_map).unwrap();
    let vote_accounts: VoteAccounts = bincode::deserialize(&data).unwrap();
    assert!(vote_accounts.staked_nodes().len() > 32);
    assert_eq!(*vote_accounts.as_ref(), vote_accounts_hash_map);
    let data = bincode::options()
        .serialize(&vote_accounts_hash_map)
        .unwrap();
    let vote_accounts: VoteAccounts = bincode::options().deserialize(&data).unwrap();
    assert_eq!(*vote_accounts.as_ref(), vote_accounts_hash_map);
}

#[test]
fn test_vote_accounts_deserialize_invalid_account() {
    let mut rng = rand::rng();
    // we'll populate the map with 1 valid and 2 invalid accounts, then ensure that we only get
    // the valid one after deserialiation
    let mut vote_accounts_hash_map = HashMap::<Pubkey, (u64, AccountSharedData)>::new();

    let valid_account = new_rand_vote_account(&mut rng, None, true);
    vote_accounts_hash_map.insert(Pubkey::new_unique(), (0xAA, valid_account.clone()));

    // bad data
    let invalid_account_data =
        AccountSharedData::new_data(42, &vec![0xFF; 42], &solana_sdk_ids::vote::id()).unwrap();
    vote_accounts_hash_map.insert(Pubkey::new_unique(), (0xBB, invalid_account_data));

    // wrong owner
    let invalid_account_key =
        AccountSharedData::new_data(42, &valid_account.data().to_vec(), &Pubkey::new_unique())
            .unwrap();
    vote_accounts_hash_map.insert(Pubkey::new_unique(), (0xCC, invalid_account_key));

    let data = bincode::serialize(&vote_accounts_hash_map).unwrap();
    let vote_accounts: VoteAccounts = bincode::deserialize(&data).unwrap();

    assert_eq!(vote_accounts.len(), 1);
    let (stake, _account) = vote_accounts.as_ref().values().next().unwrap();
    assert_eq!(*stake, 0xAA);
}

#[test]
fn test_staked_nodes() {
    let mut rng = rand::rng();
    let mut accounts: Vec<_> = new_rand_vote_accounts(&mut rng, 64).take(1024).collect();
    let mut vote_accounts = VoteAccounts::default();
    // Add vote accounts.
    for (k, (pubkey, (stake, vote_account))) in accounts.iter().enumerate() {
        vote_accounts.insert(*pubkey, vote_account.clone(), || *stake);
        if (k + 1) % 128 == 0 {
            assert_eq!(
                staked_nodes(&accounts[..k + 1]),
                *vote_accounts.staked_nodes()
            );
        }
    }
    // Remove some of the vote accounts.
    for k in 0..256 {
        let index = rng.random_range(0..accounts.len());
        let (pubkey, (_, _)) = accounts.swap_remove(index);
        vote_accounts.remove(&pubkey);
        if (k + 1) % 32 == 0 {
            assert_eq!(staked_nodes(&accounts), *vote_accounts.staked_nodes());
        }
    }
    // Modify the stakes for some of the accounts.
    for k in 0..2048 {
        let index = rng.random_range(0..accounts.len());
        let (pubkey, (stake, _)) = &mut accounts[index];
        let new_stake = rng.random_range(0..MAX_STAKE_FOR_STAKED_ACCOUNT);
        if new_stake < *stake {
            vote_accounts.sub_stake(pubkey, *stake - new_stake);
        } else {
            vote_accounts.add_stake(pubkey, new_stake - *stake);
        }
        *stake = new_stake;
        if (k + 1) % 128 == 0 {
            assert_eq!(staked_nodes(&accounts), *vote_accounts.staked_nodes());
        }
    }
    // Remove everything.
    while !accounts.is_empty() {
        let index = rng.random_range(0..accounts.len());
        let (pubkey, (_, _)) = accounts.swap_remove(index);
        vote_accounts.remove(&pubkey);
        if accounts.len() % 32 == 0 {
            assert_eq!(staked_nodes(&accounts), *vote_accounts.staked_nodes());
        }
    }
}

#[test]
fn test_staked_nodes_update() {
    let mut vote_accounts = VoteAccounts::default();

    let mut rng = rand::rng();
    let pubkey = Pubkey::new_unique();
    let node_pubkey = Pubkey::new_unique();
    let account1 = new_rand_vote_account(&mut rng, Some(node_pubkey), true);
    let vote_account1 = VoteAccount::try_from(account1).unwrap();

    // first insert
    let ret = vote_accounts.insert(pubkey, vote_account1.clone(), || 42);
    assert_eq!(ret, None);
    assert_eq!(vote_accounts.get_delegated_stake(&pubkey), 42);
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), Some(&42));

    // update with unchanged state
    let ret = vote_accounts.insert(pubkey, vote_account1.clone(), || {
        panic!("should not be called")
    });
    assert_eq!(ret, Some(vote_account1.clone()));
    assert_eq!(vote_accounts.get(&pubkey), Some(&vote_account1));
    // stake is unchanged
    assert_eq!(vote_accounts.get_delegated_stake(&pubkey), 42);
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), Some(&42));

    // update with changed state, same node pubkey
    let account2 = new_rand_vote_account(&mut rng, Some(node_pubkey), true);
    let vote_account2 = VoteAccount::try_from(account2).unwrap();
    let ret = vote_accounts.insert(pubkey, vote_account2.clone(), || {
        panic!("should not be called")
    });
    assert_eq!(ret, Some(vote_account1));
    assert_eq!(vote_accounts.get(&pubkey), Some(&vote_account2));
    // stake is unchanged
    assert_eq!(vote_accounts.get_delegated_stake(&pubkey), 42);
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), Some(&42));

    // update with new node pubkey, stake must be moved
    let new_node_pubkey = Pubkey::new_unique();
    let account3 = new_rand_vote_account(&mut rng, Some(new_node_pubkey), true);
    let vote_account3 = VoteAccount::try_from(account3).unwrap();
    let ret = vote_accounts.insert(pubkey, vote_account3, || panic!("should not be called"));
    assert_eq!(ret, Some(vote_account2));
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), None);
    assert_eq!(
        vote_accounts.staked_nodes().get(&new_node_pubkey),
        Some(&42)
    );
}

#[test]
fn test_staked_nodes_zero_stake() {
    let mut vote_accounts = VoteAccounts::default();

    let mut rng = rand::rng();
    let pubkey = Pubkey::new_unique();
    let node_pubkey = Pubkey::new_unique();
    let account1 = new_rand_vote_account(&mut rng, Some(node_pubkey), true);
    let vote_account1 = VoteAccount::try_from(account1).unwrap();

    // we call this here to initialize VoteAccounts::staked_nodes which is a OnceLock
    assert!(vote_accounts.staked_nodes().is_empty());
    let ret = vote_accounts.insert(pubkey, vote_account1.clone(), || 0);
    assert_eq!(ret, None);
    assert_eq!(vote_accounts.get_delegated_stake(&pubkey), 0);
    // ensure that we didn't add a 0 stake entry to staked_nodes
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), None);

    // update with new node pubkey, stake is 0 and should remain 0
    let new_node_pubkey = Pubkey::new_unique();
    let account2 = new_rand_vote_account(&mut rng, Some(new_node_pubkey), true);
    let vote_account2 = VoteAccount::try_from(account2).unwrap();
    let ret = vote_accounts.insert(pubkey, vote_account2, || panic!("should not be called"));
    assert_eq!(ret, Some(vote_account1));
    assert_eq!(vote_accounts.get_delegated_stake(&pubkey), 0);
    assert_eq!(vote_accounts.staked_nodes().get(&node_pubkey), None);
    assert_eq!(vote_accounts.staked_nodes().get(&new_node_pubkey), None);
}

// Asserts that returned staked-nodes are copy-on-write references.
#[test]
fn test_staked_nodes_cow() {
    let mut rng = rand::rng();
    let mut accounts = new_rand_vote_accounts(&mut rng, 64);
    // Add vote accounts.
    let mut vote_accounts = VoteAccounts::default();
    for (pubkey, (stake, vote_account)) in (&mut accounts).take(1024) {
        vote_accounts.insert(pubkey, vote_account, || stake);
    }
    let staked_nodes = vote_accounts.staked_nodes();
    let (pubkey, (more_stake, vote_account)) =
        accounts.find(|(_, (stake, _))| *stake != 0).unwrap();
    let node_pubkey = *vote_account.node_pubkey();
    vote_accounts.insert(pubkey, vote_account, || more_stake);
    assert_ne!(staked_nodes, vote_accounts.staked_nodes());
    assert_eq!(
        vote_accounts.staked_nodes()[&node_pubkey],
        more_stake + staked_nodes.get(&node_pubkey).copied().unwrap_or_default()
    );
    for (pubkey, stake) in vote_accounts.staked_nodes().iter() {
        if pubkey != &node_pubkey {
            assert_eq!(*stake, staked_nodes[pubkey]);
        } else {
            assert_eq!(
                *stake,
                more_stake + staked_nodes.get(pubkey).copied().unwrap_or_default()
            );
        }
    }
}

// Asserts that returned vote-accounts are copy-on-write references.
#[test]
fn test_vote_accounts_cow() {
    let mut rng = rand::rng();
    let mut accounts = new_rand_vote_accounts(&mut rng, 64);
    // Add vote accounts.
    let mut vote_accounts = VoteAccounts::default();
    for (pubkey, (stake, vote_account)) in (&mut accounts).take(1024) {
        vote_accounts.insert(pubkey, vote_account, || stake);
    }
    let vote_accounts_hashmap = Arc::<VoteAccountsHashMap>::from(&vote_accounts);
    assert_eq!(vote_accounts_hashmap, Arc::from(&vote_accounts));
    assert!(Arc::ptr_eq(
        &vote_accounts_hashmap,
        &Arc::from(&vote_accounts)
    ));
    let (pubkey, (more_stake, vote_account)) =
        accounts.find(|(_, (stake, _))| *stake != 0).unwrap();
    vote_accounts.insert(pubkey, vote_account.clone(), || more_stake);
    assert!(!Arc::ptr_eq(
        &vote_accounts_hashmap,
        &Arc::from(&vote_accounts)
    ));
    assert_ne!(vote_accounts_hashmap, Arc::from(&vote_accounts));
    let other = (more_stake, vote_account);
    for (pk, value) in vote_accounts.as_ref().iter() {
        if *pk != pubkey {
            assert_eq!(value, &vote_accounts_hashmap[pk]);
        } else {
            assert_eq!(value, &other);
        }
    }
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
    // Check that non-alpenglow accounts are kicked out, 2000 accounts with bls pubkey, 1000
    // accounts without.
    let num_nodes = MAX_ALPENGLOW_VOTE_ACCOUNTS + 1000;
    let vote_accounts = new_staked_vote_accounts(
        &mut rng,
        num_nodes,
        MAX_ALPENGLOW_VOTE_ACCOUNTS,
        None,
        |_| 10_000_000_000,
    );
    let new_limit = MAX_ALPENGLOW_VOTE_ACCOUNTS + 500;
    let filtered = vote_accounts.clone_and_filter_for_vat(new_limit, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), MAX_ALPENGLOW_VOTE_ACCOUNTS);
    // Check that all filtered accounts have bls pubkey.
    for (_pubkey, (_stake, vote_account)) in filtered.as_ref().iter() {
        assert!(
            vote_account
                .vote_state_view()
                .bls_pubkey_compressed()
                .is_some()
        );
    }
    // Now get only 1500 accounts, even some alpenglow accounts are kicked out.
    let new_limit = MAX_ALPENGLOW_VOTE_ACCOUNTS - 500;
    let filtered = vote_accounts.clone_and_filter_for_vat(new_limit, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert!(filtered.len() <= new_limit);
    for (_pubkey, (_stake, vote_account)) in filtered.as_ref().iter() {
        assert!(
            vote_account
                .vote_state_view()
                .bls_pubkey_compressed()
                .is_some()
        );
    }
}

#[test]
fn test_clone_and_filter_for_vat_same_stake_at_border() {
    let mut rng = rand::rng();
    // Create exactly 2 accounts more than maximum to test border truncation
    let num_accounts = MAX_ALPENGLOW_VOTE_ACCOUNTS + 2;
    let accounts = (0..num_accounts).map(|index| {
        let mut account = new_rand_vote_account(&mut rng, None, true);
        account.set_lamports(10_000_000_000);
        let vote_account = VoteAccount::try_from(account).unwrap();
        let stake = if index < MAX_ALPENGLOW_VOTE_ACCOUNTS - 10 {
            100 + index as u64
        } else {
            10 // Same stake for the last 12 accounts.
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
    let filtered = vote_accounts
        .clone_and_filter_for_vat(MAX_ALPENGLOW_VOTE_ACCOUNTS, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), MAX_ALPENGLOW_VOTE_ACCOUNTS - 10);
}

#[test]
fn test_clone_and_filter_for_vat_not_enough_lamports() {
    let mut rng = rand::rng();
    // For 10% of vote accounts, set the balance below the minimum.
    let entries_to_modify = MAX_ALPENGLOW_VOTE_ACCOUNTS / 10;
    let vote_accounts = new_staked_vote_accounts(
        &mut rng,
        MAX_ALPENGLOW_VOTE_ACCOUNTS,
        MAX_ALPENGLOW_VOTE_ACCOUNTS,
        None,
        |index| {
            if index < entries_to_modify {
                VAT_TO_BURN_PER_EPOCH - 1
            } else {
                10_000_000_000
            }
        },
    );
    let filtered =
        vote_accounts.clone_and_filter_for_vat(MAX_ALPENGLOW_VOTE_ACCOUNTS, VAT_TO_BURN_PER_EPOCH);
    assert!(filtered.len() <= MAX_ALPENGLOW_VOTE_ACCOUNTS - entries_to_modify);
}

#[test]
fn test_clone_and_filter_for_vat_empty_accounts() {
    let mut rng = rand::rng();
    let current_limit = 3000;
    let vote_accounts = new_staked_vote_accounts(
        &mut rng,
        current_limit,
        current_limit,
        Some(100), // Set all vote accounts to equal stake of 100.
        |_| 10_000_000_000,
    );
    // Since everyone has the same stake and the limit is 500 less than number of accounts,
    // all border stake peers are removed and we end up with no valid accounts.
    let filtered =
        vote_accounts.clone_and_filter_for_vat(current_limit - 500, MIN_STAKE_FOR_STAKED_ACCOUNT);
    assert_eq!(filtered.len(), 0);
}
