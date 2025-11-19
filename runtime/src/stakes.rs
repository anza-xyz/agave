//! Stakes serve as a cache of stake and vote accounts to derive
//! node stakes
#[cfg(feature = "dev-context-only-utils")]
use solana_stake_interface::state::Stake;
use {
    crate::{stake_account, stake_history::StakeHistory},
    im::HashMap as ImHashMap,
    log::error,
    num_derive::ToPrimitive,
    rayon::{prelude::*, ThreadPool},
    serde::{Deserialize, Serialize},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::utils::create_account_shared_data,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_interface::{
        program as stake_program,
        state::{Delegation, StakeActivationStatus},
    },
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::VoteStateVersions,
    std::{
        collections::HashMap,
        ops::Add,
        sync::{Arc, RwLock, RwLockReadGuard},
    },
    thiserror::Error,
};

mod serde_stakes;
pub(crate) use serde_stakes::serialize_stake_accounts_to_delegation_format;
pub use serde_stakes::SerdeStakesToStakeFormat;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid delegation: {0}")]
    InvalidDelegation(Pubkey),
    #[error(transparent)]
    InvalidStakeAccount(#[from] stake_account::Error),
    #[error("Stake account not found: {0}")]
    StakeAccountNotFound(Pubkey),
    #[error("Vote account mismatch: {0}")]
    VoteAccountMismatch(Pubkey),
    #[error("Vote account not cached: {0}")]
    VoteAccountNotCached(Pubkey),
    #[error("Vote account not found: {0}")]
    VoteAccountNotFound(Pubkey),
}

#[derive(Debug, Clone, PartialEq, Eq, ToPrimitive)]
pub enum InvalidCacheEntryReason {
    Missing,
    BadState,
    WrongOwner,
}

type StakeAccount = stake_account::StakeAccount<Delegation>;

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Default, Debug)]
pub(crate) struct StakesCache(RwLock<Stakes<StakeAccount>>);

impl StakesCache {
    pub(crate) fn new(stakes: Stakes<StakeAccount>) -> Self {
        Self(RwLock::new(stakes))
    }

    pub(crate) fn stakes(&self) -> RwLockReadGuard<'_, Stakes<StakeAccount>> {
        self.0.read().unwrap()
    }

    pub(crate) fn check_and_store(
        &self,
        pubkey: &Pubkey,
        account: &impl ReadableAccount,
        new_rate_activation_epoch: Option<Epoch>,
    ) {
        // TODO: If the account is already cached as a vote or stake account
        // but the owner changes, then this needs to evict the account from
        // the cache. see:
        // https://github.com/solana-labs/solana/pull/24200#discussion_r849935444
        let owner = account.owner();
        // Zero lamport accounts are not stored in accounts-db
        // and so should be removed from cache as well.
        if account.lamports() == 0 {
            if solana_vote_program::check_id(owner) {
                let _old_vote_account = {
                    let mut stakes = self.0.write().unwrap();
                    stakes.remove_vote_account(pubkey)
                };
            } else if stake_program::check_id(owner) {
                let mut stakes = self.0.write().unwrap();
                stakes.remove_stake_delegation(pubkey, new_rate_activation_epoch);
            }
            return;
        }
        debug_assert_ne!(account.lamports(), 0u64);
        if solana_vote_program::check_id(owner) {
            if VoteStateVersions::is_correct_size_and_initialized(account.data()) {
                match VoteAccount::try_from(create_account_shared_data(account)) {
                    Ok(vote_account) => {
                        // drop the old account after releasing the lock
                        let _old_vote_account = {
                            let mut stakes = self.0.write().unwrap();
                            stakes.upsert_vote_account(
                                pubkey,
                                vote_account,
                                new_rate_activation_epoch,
                            )
                        };
                    }
                    Err(_) => {
                        // drop the old account after releasing the lock
                        let _old_vote_account = {
                            let mut stakes = self.0.write().unwrap();
                            stakes.remove_vote_account(pubkey)
                        };
                    }
                }
            } else {
                // drop the old account after releasing the lock
                let _old_vote_account = {
                    let mut stakes = self.0.write().unwrap();
                    stakes.remove_vote_account(pubkey)
                };
            };
        } else if stake_program::check_id(owner) {
            match StakeAccount::try_from(create_account_shared_data(account)) {
                Ok(stake_account) => {
                    let mut stakes = self.0.write().unwrap();
                    stakes.upsert_stake_delegation(
                        *pubkey,
                        stake_account,
                        new_rate_activation_epoch,
                    );
                }
                Err(_) => {
                    let mut stakes = self.0.write().unwrap();
                    stakes.remove_stake_delegation(pubkey, new_rate_activation_epoch);
                }
            }
        }
    }

    pub(crate) fn activate_epoch(
        &self,
        next_epoch: Epoch,
        stake_history: StakeHistory,
        vote_accounts: VoteAccounts,
    ) {
        let mut stakes = self.0.write().unwrap();
        stakes.activate_epoch(next_epoch, stake_history, vote_accounts)
    }
}

/// The generic type T is either Delegation or StakeAccount.
/// [`Stakes<Delegation>`] is equivalent to the old code and is used for backward
/// compatibility in [`crate::bank::BankFieldsToDeserialize`].
/// But banks cache [`Stakes<StakeAccount>`] which includes the entire stake
/// account and StakeStateV2 deserialized from the account. Doing so, will remove
/// the need to load the stake account from accounts-db when working with
/// stake-delegations.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Default, Clone, PartialEq, Debug, Deserialize, Serialize)]
pub struct Stakes<T: Clone> {
    /// vote accounts
    vote_accounts: VoteAccounts,

    /// stake_delegations
    stake_delegations: ImHashMap<Pubkey, T>,

    /// unused
    unused: u64,

    /// current epoch, used to calculate current stake
    epoch: Epoch,

    /// history of staking levels
    stake_history: StakeHistory,
}

impl<T: Clone> Stakes<T> {
    pub fn vote_accounts(&self) -> &VoteAccounts {
        &self.vote_accounts
    }

    pub(crate) fn staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        self.vote_accounts.staked_nodes()
    }
}

impl Stakes<StakeAccount> {
    /// Creates a Stake<StakeAccount> from Stake<Delegation> by loading the
    /// full account state for respective stake pubkeys. get_account function
    /// should return the account at the respective slot where stakes where
    /// cached.
    pub(crate) fn new<F>(stakes: &Stakes<Delegation>, get_account: F) -> Result<Self, Error>
    where
        F: Fn(&Pubkey) -> Option<AccountSharedData> + Sync,
    {
        let stake_delegations = stakes
            .stake_delegations
            .iter()
            // im::HashMap doesn't support rayon so we manually build a temporary vector. Note this is
            // what std HashMap::par_iter() does internally too.
            .collect::<Vec<_>>()
            .into_par_iter()
            // We use fold/reduce to aggregate the results, which does a bit more work than calling
            // collect()/collect_vec_list() and then im::HashMap::from_iter(collected.into_iter()),
            // but it does it in background threads, so effectively it's faster.
            .try_fold(ImHashMap::new, |mut map, (pubkey, delegation)| {
                let Some(stake_account) = get_account(pubkey) else {
                    return Err(Error::StakeAccountNotFound(*pubkey));
                };

                // Assert that all valid vote-accounts referenced in stake delegations are already
                // contained in `stakes.vote_account`.
                let voter_pubkey = &delegation.voter_pubkey;
                if stakes.vote_accounts.get(voter_pubkey).is_none() {
                    if let Some(account) = get_account(voter_pubkey) {
                        if VoteStateVersions::is_correct_size_and_initialized(account.data())
                            && VoteAccount::try_from(account.clone()).is_ok()
                        {
                            error!("vote account not cached: {voter_pubkey}, {account:?}");
                            return Err(Error::VoteAccountNotCached(*voter_pubkey));
                        }
                    }
                }

                let stake_account = StakeAccount::try_from(stake_account)?;
                // Sanity check that the delegation is consistent with what is
                // stored in the account.
                if stake_account.delegation() == delegation {
                    map.insert(*pubkey, stake_account);
                    Ok(map)
                } else {
                    Err(Error::InvalidDelegation(*pubkey))
                }
            })
            .try_reduce(ImHashMap::new, |a, b| Ok(a.union(b)))?;

        // Assert that cached vote accounts are consistent with accounts-db.
        //
        // This currently includes ~5500 accounts, parallelizing brings minor
        // (sub 2s) improvements.
        for (pubkey, vote_account) in stakes.vote_accounts.iter() {
            let Some(account) = get_account(pubkey) else {
                return Err(Error::VoteAccountNotFound(*pubkey));
            };
            let vote_account = vote_account.account();
            if vote_account != &account {
                error!("vote account mismatch: {pubkey}, {vote_account:?}, {account:?}");
                return Err(Error::VoteAccountMismatch(*pubkey));
            }
        }

        Ok(Self {
            vote_accounts: stakes.vote_accounts.clone(),
            stake_delegations,
            unused: stakes.unused,
            epoch: stakes.epoch,
            stake_history: stakes.stake_history.clone(),
        })
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        epoch: Epoch,
        vote_accounts: VoteAccounts,
        stake_delegations: ImHashMap<Pubkey, StakeAccount>,
    ) -> Self {
        Self {
            vote_accounts,
            stake_delegations,
            unused: 0,
            epoch,
            stake_history: StakeHistory::default(),
        }
    }

    pub(crate) fn history(&self) -> &StakeHistory {
        &self.stake_history
    }

    pub(crate) fn calculate_activated_stake(
        &self,
        next_epoch: Epoch,
        thread_pool: &ThreadPool,
        new_rate_activation_epoch: Option<Epoch>,
        stake_delegations: &[(&Pubkey, &StakeAccount)],
    ) -> (StakeHistory, VoteAccounts) {
        // Wrap up the prev epoch by adding new stake history entry for the
        // prev epoch.
        let stake_history_entry = thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .fold(
                    StakeActivationStatus::default,
                    |acc, (_stake_pubkey, stake_account)| {
                        let delegation = stake_account.delegation();
                        acc + delegation.stake_activating_and_deactivating(
                            self.epoch,
                            &self.stake_history,
                            new_rate_activation_epoch,
                        )
                    },
                )
                .reduce(StakeActivationStatus::default, Add::add)
        });
        let mut stake_history = self.stake_history.clone();
        stake_history.add(self.epoch, stake_history_entry);
        // Refresh the stake distribution of vote accounts for the next epoch,
        // using new stake history.
        let vote_accounts = refresh_vote_accounts(
            thread_pool,
            next_epoch,
            &self.vote_accounts,
            stake_delegations,
            &stake_history,
            new_rate_activation_epoch,
        );
        (stake_history, vote_accounts)
    }

    pub(crate) fn activate_epoch(
        &mut self,
        next_epoch: Epoch,
        stake_history: StakeHistory,
        vote_accounts: VoteAccounts,
    ) {
        self.epoch = next_epoch;
        self.stake_history = stake_history;
        self.vote_accounts = vote_accounts;
    }

    /// Sum the stakes that point to the given voter_pubkey
    fn calculate_stake(
        stake_delegations: &ImHashMap<Pubkey, StakeAccount>,
        voter_pubkey: &Pubkey,
        epoch: Epoch,
        stake_history: &StakeHistory,
        new_rate_activation_epoch: Option<Epoch>,
    ) -> u64 {
        stake_delegations
            .values()
            .map(StakeAccount::delegation)
            .filter(|delegation| &delegation.voter_pubkey == voter_pubkey)
            .map(|delegation| delegation.stake(epoch, stake_history, new_rate_activation_epoch))
            .sum()
    }

    fn remove_vote_account(&mut self, vote_pubkey: &Pubkey) -> Option<VoteAccount> {
        self.vote_accounts.remove(vote_pubkey).map(|(_, a)| a)
    }

    fn remove_stake_delegation(
        &mut self,
        stake_pubkey: &Pubkey,
        new_rate_activation_epoch: Option<Epoch>,
    ) {
        if let Some(stake_account) = self.stake_delegations.remove(stake_pubkey) {
            let removed_delegation = stake_account.delegation();
            let removed_stake = removed_delegation.stake(
                self.epoch,
                &self.stake_history,
                new_rate_activation_epoch,
            );
            self.vote_accounts
                .sub_stake(&removed_delegation.voter_pubkey, removed_stake);
        }
    }

    fn upsert_vote_account(
        &mut self,
        vote_pubkey: &Pubkey,
        vote_account: VoteAccount,
        new_rate_activation_epoch: Option<Epoch>,
    ) -> Option<VoteAccount> {
        debug_assert_ne!(vote_account.lamports(), 0u64);

        let stake_delegations = &self.stake_delegations;
        self.vote_accounts.insert(*vote_pubkey, vote_account, || {
            Self::calculate_stake(
                stake_delegations,
                vote_pubkey,
                self.epoch,
                &self.stake_history,
                new_rate_activation_epoch,
            )
        })
    }

    fn upsert_stake_delegation(
        &mut self,
        stake_pubkey: Pubkey,
        stake_account: StakeAccount,
        new_rate_activation_epoch: Option<Epoch>,
    ) {
        debug_assert_ne!(stake_account.lamports(), 0u64);
        let delegation = stake_account.delegation();
        let voter_pubkey = delegation.voter_pubkey;
        let stake = delegation.stake(self.epoch, &self.stake_history, new_rate_activation_epoch);
        match self.stake_delegations.insert(stake_pubkey, stake_account) {
            None => self.vote_accounts.add_stake(&voter_pubkey, stake),
            Some(old_stake_account) => {
                let old_delegation = old_stake_account.delegation();
                let old_voter_pubkey = old_delegation.voter_pubkey;
                let old_stake = old_delegation.stake(
                    self.epoch,
                    &self.stake_history,
                    new_rate_activation_epoch,
                );
                if voter_pubkey != old_voter_pubkey || stake != old_stake {
                    self.vote_accounts.sub_stake(&old_voter_pubkey, old_stake);
                    self.vote_accounts.add_stake(&voter_pubkey, stake);
                }
            }
        }
    }

    /// Returns a reference to the map of stake delegations.
    ///
    /// # Performance
    ///
    /// `[im::HashMap]` is a [hash array mapped trie (HAMT)][hamt], which means
    /// that inserts, deletions and lookups are average-case O(1) and
    /// worst-case O(log n). However, the performance of iterations is poor due
    /// to depth-first traversal and jumps. Currently it's also impossible to
    /// iterate over it with [`rayon`].
    ///
    /// [hamt]: https://en.wikipedia.org/wiki/Hash_array_mapped_trie
    pub(crate) fn stake_delegations(&self) -> &ImHashMap<Pubkey, StakeAccount> {
        &self.stake_delegations
    }

    /// Collects stake delegations into a vector, which then can be used for
    /// parallel iteration with [`rayon`].
    ///
    /// # Performance
    ///
    /// The execution of this method takes ~200ms and it collects elements of
    /// the [`im::HashMap`], which is a [hash array mapped trie (HAMT)][hamt],
    /// so that operation involves a depth-first traversal with jumps. However,
    /// it's still a reasonable tradeoff if the caller iterates over these
    /// elements.
    ///
    /// [hamt]: https://en.wikipedia.org/wiki/Hash_array_mapped_trie
    pub(crate) fn stake_delegations_vec(&self) -> Vec<(&Pubkey, &StakeAccount)> {
        self.stake_delegations.iter().collect()
    }

    pub(crate) fn highest_staked_node(&self) -> Option<&Pubkey> {
        let vote_account = self.vote_accounts.find_max_by_delegated_stake()?;
        Some(vote_account.node_pubkey())
    }
}

/// This conversion is very memory intensive so should only be used in
/// development contexts.
#[cfg(feature = "dev-context-only-utils")]
impl From<Stakes<StakeAccount>> for Stakes<Delegation> {
    fn from(stakes: Stakes<StakeAccount>) -> Self {
        let stake_delegations = stakes
            .stake_delegations
            .into_iter()
            .map(|(pubkey, stake_account)| (pubkey, *stake_account.delegation()))
            .collect();
        Self {
            vote_accounts: stakes.vote_accounts,
            stake_delegations,
            unused: stakes.unused,
            epoch: stakes.epoch,
            stake_history: stakes.stake_history,
        }
    }
}

/// This conversion is very memory intensive so should only be used in
/// development contexts.
#[cfg(feature = "dev-context-only-utils")]
impl From<Stakes<StakeAccount>> for Stakes<Stake> {
    fn from(stakes: Stakes<StakeAccount>) -> Self {
        let stake_delegations = stakes
            .stake_delegations
            .into_iter()
            .map(|(pubkey, stake_account)| (pubkey, *stake_account.stake()))
            .collect();
        Self {
            vote_accounts: stakes.vote_accounts,
            stake_delegations,
            unused: stakes.unused,
            epoch: stakes.epoch,
            stake_history: stakes.stake_history,
        }
    }
}

/// This conversion is memory intensive so should only be used in development
/// contexts.
#[cfg(feature = "dev-context-only-utils")]
impl From<Stakes<Stake>> for Stakes<Delegation> {
    fn from(stakes: Stakes<Stake>) -> Self {
        let stake_delegations = stakes
            .stake_delegations
            .into_iter()
            .map(|(pubkey, stake)| (pubkey, stake.delegation))
            .collect();
        Self {
            vote_accounts: stakes.vote_accounts,
            stake_delegations,
            unused: stakes.unused,
            epoch: stakes.epoch,
            stake_history: stakes.stake_history,
        }
    }
}

fn refresh_vote_accounts(
    thread_pool: &ThreadPool,
    epoch: Epoch,
    vote_accounts: &VoteAccounts,
    stake_delegations: &[(&Pubkey, &StakeAccount)],
    stake_history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
) -> VoteAccounts {
    type StakesHashMap = HashMap</*voter:*/ Pubkey, /*stake:*/ u64>;
    fn merge(mut stakes: StakesHashMap, other: StakesHashMap) -> StakesHashMap {
        if stakes.len() < other.len() {
            return merge(other, stakes);
        }
        for (pubkey, stake) in other {
            *stakes.entry(pubkey).or_default() += stake;
        }
        stakes
    }
    let delegated_stakes = thread_pool.install(|| {
        stake_delegations
            .par_iter()
            .fold(
                HashMap::default,
                |mut delegated_stakes, (_stake_pubkey, stake_account)| {
                    let delegation = stake_account.delegation();
                    let entry = delegated_stakes.entry(delegation.voter_pubkey).or_default();
                    *entry += delegation.stake(epoch, stake_history, new_rate_activation_epoch);
                    delegated_stakes
                },
            )
            .reduce(HashMap::default, merge)
    });
    vote_accounts
        .iter()
        .map(|(&vote_pubkey, vote_account)| {
            let delegated_stake = delegated_stakes
                .get(&vote_pubkey)
                .copied()
                .unwrap_or_default();
            (vote_pubkey, (delegated_stake, vote_account.clone()))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::stake_utils,
        rayon::ThreadPoolBuilder,
        solana_account::WritableAccount,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_stake_interface::{self as stake, state::StakeStateV2},
        solana_vote_interface::state::VoteStateV4,
        solana_vote_program::vote_state,
    };

    //  set up some dummies for a staked node     ((     vote      )  (     stake     ))
    pub(crate) fn create_staked_node_accounts(
        stake: u64,
    ) -> ((Pubkey, AccountSharedData), (Pubkey, AccountSharedData)) {
        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();
        let vote_account = vote_state::create_v4_account_with_authorized(
            &node_pubkey,
            &vote_pubkey,
            &vote_pubkey,
            None,
            0,
            1,
        );
        let stake_pubkey = solana_pubkey::new_rand();
        (
            (vote_pubkey, vote_account),
            (
                stake_pubkey,
                create_stake_account(stake, &vote_pubkey, &stake_pubkey),
            ),
        )
    }

    //   add stake to a vote_pubkey                               (   stake    )
    pub(crate) fn create_stake_account(
        stake: u64,
        vote_pubkey: &Pubkey,
        stake_pubkey: &Pubkey,
    ) -> AccountSharedData {
        let node_pubkey = solana_pubkey::new_rand();
        stake_utils::create_stake_account(
            stake_pubkey,
            vote_pubkey,
            &vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                vote_pubkey,
                vote_pubkey,
                None,
                0,
                1,
            ),
            &Rent::free(),
            stake,
        )
    }

    #[test]
    fn test_stakes_basic() {
        for i in 0..4 {
            let stakes_cache = StakesCache::new(Stakes {
                epoch: i,
                ..Stakes::default()
            });

            let ((vote_pubkey, vote_account), (stake_pubkey, mut stake_account)) =
                create_staked_node_accounts(10);

            stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
            let stake = stake_account
                .deserialize_data::<StakeStateV2>()
                .unwrap()
                .stake()
                .unwrap();
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    stake.stake(i, &StakeHistory::default(), None)
                );
            }

            stake_account.set_lamports(42);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    stake.stake(i, &StakeHistory::default(), None)
                ); // stays old stake, because only 10 is activated
            }

            // activate more
            let mut stake_account =
                create_stake_account(42, &vote_pubkey, &solana_pubkey::new_rand());
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
            let stake = stake_account
                .deserialize_data::<StakeStateV2>()
                .unwrap()
                .stake()
                .unwrap();
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    stake.stake(i, &StakeHistory::default(), None)
                ); // now stake of 42 is activated
            }

            stake_account.set_lamports(0);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
            }
        }
    }

    #[test]
    fn test_stakes_highest() {
        let stakes_cache = StakesCache::default();

        assert_eq!(stakes_cache.stakes().highest_staked_node(), None);

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);

        let ((vote11_pubkey, vote11_account), (stake11_pubkey, stake11_account)) =
            create_staked_node_accounts(20);

        stakes_cache.check_and_store(&vote11_pubkey, &vote11_account, None);
        stakes_cache.check_and_store(&stake11_pubkey, &stake11_account, None);

        let vote11_node_pubkey = VoteStateV4::deserialize(vote11_account.data(), &vote11_pubkey)
            .unwrap()
            .node_pubkey;

        let highest_staked_node = stakes_cache.stakes().highest_staked_node().copied();
        assert_eq!(highest_staked_node, Some(vote11_node_pubkey));
    }

    #[test]
    fn test_stakes_vote_account_disappear_reappear() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });

        let ((vote_pubkey, mut vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 10);
        }

        vote_account.set_lamports(0);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        vote_account.set_lamports(1);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 10);
        }

        // Vote account too big
        let cache_data = vote_account.data().to_vec();
        let mut pushed = vote_account.data().to_vec();
        pushed.push(0);
        vote_account.set_data(pushed);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        // Vote account uninitialized
        vote_account.set_data(vec![0; VoteStateV4::size_of()]);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        vote_account.set_data(cache_data);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 10);
        }
    }

    #[test]
    fn test_stakes_change_delegate() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        let ((vote_pubkey2, vote_account2), (_stake_pubkey2, stake_account2)) =
            create_staked_node_accounts(10);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
        stakes_cache.check_and_store(&vote_pubkey2, &vote_account2, None);

        // delegates to vote_pubkey
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);

        let stake = stake_account
            .deserialize_data::<StakeStateV2>()
            .unwrap()
            .stake()
            .unwrap();

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                stake.stake(stakes.epoch, &stakes.stake_history, None)
            );
            assert!(vote_accounts.get(&vote_pubkey2).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey2), 0);
        }

        // delegates to vote_pubkey2
        stakes_cache.check_and_store(&stake_pubkey, &stake_account2, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
            assert!(vote_accounts.get(&vote_pubkey2).is_some());
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey2),
                stake.stake(stakes.epoch, &stakes.stake_history, None)
            );
        }
    }
    #[test]
    fn test_stakes_multiple_stakers() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        let stake_pubkey2 = solana_pubkey::new_rand();
        let stake_account2 = create_stake_account(10, &vote_pubkey, &stake_pubkey2);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);

        // delegates to vote_pubkey
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
        stakes_cache.check_and_store(&stake_pubkey2, &stake_account2, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 20);
        }
    }

    #[test]
    fn test_activate_epoch() {
        let stakes_cache = StakesCache::default();

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
        let stake = stake_account
            .deserialize_data::<StakeStateV2>()
            .unwrap()
            .stake()
            .unwrap();

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                stake.stake(stakes.epoch, &stakes.stake_history, None)
            );
        }
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let next_epoch = 3;
        let (stake_history, vote_accounts) = {
            let stakes = stakes_cache.stakes();
            let stake_delegations = stakes.stake_delegations_vec();
            stakes.calculate_activated_stake(next_epoch, &thread_pool, None, &stake_delegations)
        };
        stakes_cache.activate_epoch(next_epoch, stake_history, vote_accounts);
        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                stake.stake(stakes.epoch, &stakes.stake_history, None)
            );
        }
    }

    #[test]
    fn test_stakes_not_delegate() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 10);
        }

        // not a stake account, and whacks above entry
        stakes_cache.check_and_store(
            &stake_pubkey,
            &AccountSharedData::new(1, 0, &stake::program::id()),
            None,
        );
        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }
    }
}
