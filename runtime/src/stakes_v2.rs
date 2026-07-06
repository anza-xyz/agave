use {
    crate::{
        stake_account,
        stake_history::StakeHistory,
        stakes::{DeserializableStakes, Error},
    },
    ahash::{AHashMap, RandomState as AHashRandomState},
    indexmap::IndexMap,
    rayon::iter::{
        IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::utils::create_account_shared_data,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_interface::{program as stake_program, state::Delegation},
    solana_vote::vote_account::{VoteAccount, VoteAccounts, VoteAccountsHashMap},
    solana_vote_interface::state::VoteStateVersions,
    std::{
        ops::{Deref, DerefMut},
        sync::{Arc, RwLock, RwLockReadGuard},
    },
};

type StakeAccount = stake_account::StakeAccount<Delegation>;

/// Per-bank state consisting of currently processed epoch and stake history.
#[derive(Debug, Clone)]
pub(crate) struct StakesCacheV2State {
    epoch: Epoch,
    stake_history: StakeHistory,
}

impl StakesCacheV2State {
    /// Returns the stake history associated with this state.
    pub(crate) fn stake_history(&self) -> &StakeHistory {
        &self.stake_history
    }
}

/// Stake delegation changes recorded by one unrooted bank/fork.
type ForkDelta = AHashMap<Pubkey, Option<Arc<StakeAccount>>>;

type RootEntriesInner = IndexMap<Pubkey, Arc<StakeAccount>, AHashRandomState>;

/// A dense index map of all rooted stake delegations.
#[derive(Debug)]
struct RootEntries(RootEntriesInner);

impl Default for RootEntries {
    fn default() -> Self {
        Self(IndexMap::with_hasher(AHashRandomState::new()))
    }
}

impl Deref for RootEntries {
    type Target = RootEntriesInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RootEntries {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl IntoIterator for RootEntries {
    type Item = (Pubkey, Arc<StakeAccount>);
    type IntoIter = indexmap::map::IntoIter<Pubkey, Arc<StakeAccount>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
impl RootEntries {
    /// Creates an empty [`RootEntries`] map with at least the specified
    /// capacity.
    fn with_capacity(capacity: usize) -> Self {
        Self(IndexMap::with_capacity_and_hasher(
            capacity,
            AHashRandomState::new(),
        ))
    }
}

type FrontierOverridesInner = AHashMap<usize, Option<Arc<StakeAccount>>>;

/// Aggregated frontier changes to stake delegations already present in
/// [`RootEntries`].
///
/// A [`FrontierOverrides`] value is built for one frontier query by walking
/// fork deltas from unrooted ancestors and the current bank in ancestor order.
/// Entries are keyed by rooted entry index so query iteration does not need to
/// hash each rooted stake pubkey again.
#[derive(Debug, Default)]
struct FrontierOverrides(FrontierOverridesInner);

impl FrontierOverrides {
    /// Returns the stake pubkey and account, applying any override.
    ///
    /// If the override contains an update for this entry's stake pubkey, the
    /// updated value is returned. If the override marks the stake as removed,
    /// `None` is returned. Otherwise the rooted value is returned.
    fn apply_override<'a>(
        &'a self,
        root_index: usize,
        stake_pubkey: &'a Pubkey,
        stake_account: &'a Arc<StakeAccount>,
    ) -> Option<(&'a Pubkey, &'a StakeAccount)> {
        match self.0.get(&root_index) {
            Some(Some(stake_account)) => Some((stake_pubkey, stake_account.as_ref())),
            Some(None) => None,
            None => Some((stake_pubkey, stake_account.as_ref())),
        }
    }
}

impl Deref for FrontierOverrides {
    type Target = FrontierOverridesInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FrontierOverrides {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Aggregated frontier inserts for stake delegations not present in
/// [`RootEntries`].
///
/// A [`FrontierInserts`] value is built for one frontier query from fork deltas
/// across unrooted ancestors and the current bank. These entries have no rooted
/// index, so they remain keyed by stake pubkey.
type FrontierInserts = IndexMap<Pubkey, Arc<StakeAccount>, AHashRandomState>;

#[derive(Debug)]
struct StakeDelegationIndexInner {
    /// Rooted stake entries.
    root_entries: RootEntries,
}

/// Index of rooted stake delegations, shared across banks.
#[derive(Debug)]
struct StakeDelegationIndex(RwLock<StakeDelegationIndexInner>);

/// Merged stake-delegation view for one bank frontier.
///
/// The view combines rooted stake delegations with fork deltas from unrooted
/// ancestors and the current bank.
#[derive(Debug)]
pub(crate) struct FrontierQuery<'a> {
    /// Lock guard holding the index of rooted stake delegations.
    inner: RwLockReadGuard<'a, StakeDelegationIndexInner>,
    /// Aggregated frontier changes for rooted stake delegations.
    overrides: FrontierOverrides,
    /// Aggregated frontier inserts absent from rooted stake delegations.
    inserts: FrontierInserts,
    /// Number of rooted entries that are removed by frontier overrides.
    num_removed: usize,
}

impl<'a> FrontierQuery<'a> {
    /// Returns the total number of stake delegation entries, including rooted
    /// entries, frontier-only inserts, without excluding overlay-removed
    /// roots. The returned number is equivalent to the number of elements
    /// yielded by [`FrontierQuery::par_iter_unfiltered`], including the `None`
    /// elements.
    pub(crate) fn len_unfiltered(&self) -> usize {
        self.inner
            .root_entries
            .len()
            .wrapping_add(self.inserts.len())
    }

    /// Returns the total number of stake delegation entries, including rooted
    /// entries, frontier-only inserts, and excluding overlay-removed roots.
    /// The returned number is equivalent to the number of elements yielded by
    /// [`FrontierQuery::par_iter_filtered`], and does not include the `None`
    /// elements.
    pub(crate) fn len_filtered(&self) -> usize {
        self.inner
            .root_entries
            .len()
            .wrapping_sub(self.num_removed)
            .wrapping_add(self.inserts.len())
    }

    /// Returns an indexed parallel iterator (with a known size) over all stake
    /// delegations.
    ///
    /// Each item is `Option<(&Pubkey, &StakeAccount)>` — `None` for rooted
    /// entries that were removed by the overlay.
    ///
    /// # Performance
    ///
    /// Known size of the iterator makes it a good choice for usage that
    /// involves allocating collections.
    pub(crate) fn par_iter_unfiltered(
        &'a self,
    ) -> impl IndexedParallelIterator<Item = Option<(&'a Pubkey, &'a StakeAccount)>> {
        self.inner
            .root_entries
            .par_iter()
            .enumerate()
            .map(|(root_index, (stake_pubkey, stake_account))| {
                self.overrides
                    .apply_override(root_index, stake_pubkey, stake_account)
            })
            .chain(
                self.inserts
                    .par_iter()
                    .map(|(stake_pubkey, stake_account)| {
                        Some((stake_pubkey, stake_account.as_ref()))
                    }),
            )
    }

    /// Returns a parallel iterator (with an unknown size) over all valid stake
    /// delegations, filtering out roots removed by the overlay.
    ///
    /// # Performance
    ///
    /// Unknown size of the iterator makes it a bad choice for usage that
    /// involves allocating collections, where
    /// [`FrontierQuery::par_iter_unfiltered`] should be used instead.
    pub(crate) fn par_iter_filtered(
        &'a self,
    ) -> impl ParallelIterator<Item = (&'a Pubkey, &'a StakeAccount)> {
        self.par_iter_unfiltered()
            .filter_map(|maybe_stake| maybe_stake)
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl<'a> FrontierQuery<'a> {
    /// Returns an iterator (with a known size) over all stake delegations.
    ///
    /// Each item is `Option<(&Pubkey, &StakeAccount)>` — `None` for rooted
    /// entries that were removed by the overlay.
    ///
    /// # Performance
    ///
    /// Known size of the iterator makes it a good choice for usage that
    /// involves allocating collections.
    pub(crate) fn iter_unfiltered(
        &'a self,
    ) -> impl Iterator<Item = Option<(&'a Pubkey, &'a StakeAccount)>> {
        self.inner
            .root_entries
            .iter()
            .enumerate()
            .map(|(root_index, (stake_pubkey, stake_account))| {
                self.overrides
                    .apply_override(root_index, stake_pubkey, stake_account)
            })
            .chain(
                self.inserts.iter().map(|(stake_pubkey, stake_account)| {
                    Some((stake_pubkey, stake_account.as_ref()))
                }),
            )
    }

    /// Returns an iterator (with an unknown size) over all valid stake
    /// delegations, filtering out roots removed by the overlay.
    ///
    /// # Performance
    ///
    /// Unknown size of the iterator makes it a bad choice for usage that
    /// involves allocating collections, where
    /// [`FrontierQuery::iter_unfiltered`] should be used instead.
    pub(crate) fn iter_filtered(&'a self) -> impl Iterator<Item = (&'a Pubkey, &'a StakeAccount)> {
        self.iter_unfiltered().flatten()
    }
}

impl StakeDelegationIndex {
    /// Applies rooted stake delegation deltas (in ancestor order) to the index,
    /// updating or removing entries.
    fn apply_rooted_deltas<D, I>(&self, deltas_in_ancestor_order: D)
    where
        D: IntoIterator<Item = I>,
        I: IntoIterator<Item = (Pubkey, Option<Arc<StakeAccount>>)>,
    {
        let mut inner = self.0.write().unwrap();
        for delta in deltas_in_ancestor_order {
            for (stake_pubkey, maybe_stake_account) in delta {
                match maybe_stake_account {
                    Some(stake_account) => {
                        inner.root_entries.insert(stake_pubkey, stake_account);
                    }
                    None => {
                        inner.root_entries.swap_remove(&stake_pubkey);
                    }
                }
            }
        }
    }
}

/// Stakes cache stored by a bank. It consists of:
///
/// - A reference to [`StakeDelegationIndex`], that is shared between all
///   banks.
/// - A local [`ForkDelta`].
/// - A local [`StakesCacheV2State`].
#[derive(Debug)]
pub(crate) struct StakesCacheV2 {
    stake_delegation_index: Arc<StakeDelegationIndex>,
    fork_delta: RwLock<ForkDelta>,
    state: RwLock<StakesCacheV2State>,
}

impl StakesCacheV2 {
    /// Creates a new `StakesCacheV2` from genesis accounts, extracting vote
    /// accounts and stake delegations to build the initial rooted index.
    pub(crate) fn new_from_accounts_for_genesis<'a, T: ReadableAccount + 'a>(
        accounts: impl IntoIterator<Item = (&'a Pubkey, &'a T)>,
    ) -> Self {
        let epoch = 0;
        let stake_history = StakeHistory::default();
        let mut vote_accounts = VoteAccountsHashMap::default();
        let mut delegated_stakes: AHashMap<Pubkey, u64> = AHashMap::default();
        let mut root_entries = RootEntries::default();

        for (pubkey, account) in accounts {
            if account.lamports() == 0 {
                continue;
            }

            if solana_vote_program::check_id(account.owner()) {
                if VoteStateVersions::is_correct_size_and_initialized(account.data())
                    && let Ok(vote_account) =
                        VoteAccount::try_from(create_account_shared_data(account))
                {
                    vote_accounts.insert(*pubkey, (0, vote_account));
                }
            } else if stake_program::check_id(account.owner())
                && let Ok(stake_account) =
                    StakeAccount::try_from(create_account_shared_data(account))
            {
                let delegation = stake_account.delegation();
                #[expect(deprecated, reason = "we still use the legacy stake calculation")]
                let stake = delegation.stake(epoch, &stake_history, None);
                *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
                root_entries.insert(*pubkey, Arc::new(stake_account));
            }
        }

        let mut vote_accounts = VoteAccounts::from(Arc::new(vote_accounts));
        for (vote_pubkey, stake) in delegated_stakes {
            vote_accounts.add_stake(&vote_pubkey, stake);
        }

        Self {
            stake_delegation_index: Arc::new(StakeDelegationIndex(RwLock::new(
                StakeDelegationIndexInner { root_entries },
            ))),
            fork_delta: RwLock::new(ForkDelta::default()),
            state: RwLock::new(StakesCacheV2State {
                epoch,
                stake_history,
            }),
        }
    }

    /// Loads `StakesCacheV2` from deserialized stake delegations, fetching each
    /// full stake account via `get_account` and verifying delegation consistency.
    pub(crate) fn load_from_deserialized_delegations<F>(
        stakes: DeserializableStakes<Delegation>,
        get_account: F,
    ) -> Result<Self, Error>
    where
        F: Fn(&Pubkey) -> Option<AccountSharedData> + Sync,
    {
        let root_entries = stakes
            .stake_delegations
            .into_par_iter()
            .try_fold(
                RootEntries::default,
                |mut root_entries, (pubkey, delegation)| {
                    let Some(stake_account) = get_account(&pubkey) else {
                        return Err(Error::StakeAccountNotFound(pubkey));
                    };

                    let stake_account = StakeAccount::try_from(stake_account)?;
                    if stake_account.delegation() == &delegation {
                        root_entries.insert(pubkey, Arc::new(stake_account));
                        Ok(root_entries)
                    } else {
                        Err(Error::InvalidDelegation(pubkey))
                    }
                },
            )
            .try_reduce(
                RootEntries::default,
                |mut root_entries, other_root_entries| {
                    root_entries.extend(other_root_entries);
                    Ok(root_entries)
                },
            )?;

        let DeserializableStakes {
            epoch,
            stake_history,
            ..
        } = stakes;

        Ok(Self {
            stake_delegation_index: Arc::new(StakeDelegationIndex(RwLock::new(
                StakeDelegationIndexInner { root_entries },
            ))),
            fork_delta: RwLock::new(ForkDelta::default()),
            state: RwLock::new(StakesCacheV2State {
                epoch,
                stake_history,
            }),
        })
    }

    /// Creates a new `StakesCacheV2` from a parent bank, sharing the rooted
    /// index but starting with an empty fork delta.
    pub(crate) fn new_from_parent(parent: &Self) -> Self {
        let stake_delegation_index = Arc::clone(&parent.stake_delegation_index);
        let state = parent.state.read().unwrap().clone();
        Self {
            stake_delegation_index,
            fork_delta: RwLock::new(ForkDelta::default()),
            state: RwLock::new(state),
        }
    }

    /// Builds a `FrontierQuery` by merging rooted entries with all fork deltas
    /// in ancestor order, applying fork delta updates and collecting
    /// frontier-only inserts.
    pub(crate) fn frontier_query<'a>(
        &self,
        caches_in_ancestor_order: impl IntoIterator<Item = &'a Self>,
    ) -> FrontierQuery<'_> {
        let inner = self.stake_delegation_index.0.read().unwrap();
        let mut overrides = FrontierOverrides::default();
        let mut inserts = FrontierInserts::default();
        let mut num_removed: usize = 0;
        let mut insert_to_frontier =
            |stake_pubkey: &Pubkey, stake_account: &Option<Arc<StakeAccount>>| {
                if let Some((root_index, _, _)) = inner.root_entries.get_full(stake_pubkey) {
                    let previous_stake_account =
                        overrides.insert(root_index, stake_account.clone());
                    if stake_account.is_none() {
                        if !matches!(previous_stake_account, Some(None)) {
                            num_removed = num_removed.wrapping_add(1);
                        }
                    } else if matches!(previous_stake_account, Some(None)) {
                        num_removed = num_removed.wrapping_sub(1);
                    }
                } else {
                    match stake_account {
                        Some(stake_account) => {
                            inserts.insert(*stake_pubkey, Arc::clone(stake_account));
                        }
                        None => {
                            inserts.swap_remove(stake_pubkey);
                        }
                    }
                }
            };
        for cache in caches_in_ancestor_order {
            let fork_delta = cache.fork_delta.read().unwrap();
            for (stake_pubkey, stake_account) in fork_delta.iter() {
                insert_to_frontier(stake_pubkey, stake_account);
            }
        }
        {
            let fork_delta = self.fork_delta.read().unwrap();
            for (stake_pubkey, stake_account) in fork_delta.iter() {
                insert_to_frontier(stake_pubkey, stake_account);
            }
        }

        FrontierQuery {
            inner,
            overrides,
            inserts,
            num_removed,
        }
    }

    /// Applies rooted stake delegation deltas from ancestor caches and this
    /// bank's own fork delta, updating the shared rooted index.
    pub(crate) fn apply_rooted_stake_delegation_deltas<'a>(
        &'a self,
        caches_in_ancestor_order: impl IntoIterator<Item = &'a Self>,
    ) {
        let deltas_in_ancestor_order = caches_in_ancestor_order
            .into_iter()
            .chain([self])
            .filter_map(|cache| {
                let mut fork_delta = cache.fork_delta.write().unwrap();
                (!fork_delta.is_empty()).then(|| std::mem::take(&mut *fork_delta))
            });
        self.stake_delegation_index
            .apply_rooted_deltas(deltas_in_ancestor_order);
    }

    /// Checks if an account is a stake program account and either stores a new
    /// delegation or removes an existing one if the account is empty or invalid.
    pub(crate) fn check_and_store(&self, pubkey: &Pubkey, account: &impl ReadableAccount) {
        if !stake_program::check_id(account.owner()) {
            return;
        }
        if account.lamports() == 0 {
            self.remove_stake_delegation(pubkey);
            return;
        }
        match StakeAccount::try_from(create_account_shared_data(account)) {
            Ok(stake_account) => self.upsert_stake_delegation(*pubkey, stake_account),
            Err(_) => self.remove_stake_delegation(pubkey),
        }
    }

    /// Inserts or updates a stake delegation in the fork delta, making it visible
    /// to subsequent frontier queries.
    pub(crate) fn upsert_stake_delegation(&self, pubkey: Pubkey, stake_account: StakeAccount) {
        self.fork_delta
            .write()
            .unwrap()
            .insert(pubkey, Some(Arc::new(stake_account)));
    }

    /// Removes a stake delegation from the fork delta. Subsequent frontier queries
    /// will not include this stake.
    pub(crate) fn remove_stake_delegation(&self, pubkey: &Pubkey) {
        self.fork_delta.write().unwrap().insert(*pubkey, None);
    }

    /// Returns the current epoch and stake history associated with this cache.
    pub(crate) fn state(&self) -> std::sync::RwLockReadGuard<'_, StakesCacheV2State> {
        self.state.read().unwrap()
    }

    /// Advances the epoch to `next_epoch` and updates the stake history.
    ///
    /// This method is called at epoch boundary to finalize rooted deltas and
    /// update the stake history for stake-weighted calculations.
    pub(crate) fn activate_epoch(&self, next_epoch: Epoch, stake_history: StakeHistory) {
        let mut state = self.state.write().unwrap();
        state.epoch = next_epoch;
        state.stake_history = stake_history;
    }
}

#[cfg(test)]
impl StakesCacheV2 {
    pub(crate) fn new_from_accounts(
        accounts: impl ExactSizeIterator<Item = (Pubkey, StakeAccount)>,
        epoch: Epoch,
    ) -> Self {
        let mut root_entries = RootEntries::with_capacity(accounts.len());

        for (pubkey, account) in accounts {
            root_entries.insert(pubkey, Arc::new(account));
        }

        Self {
            stake_delegation_index: Arc::new(StakeDelegationIndex(RwLock::new(
                StakeDelegationIndexInner { root_entries },
            ))),
            fork_delta: RwLock::new(ForkDelta::default()),
            state: RwLock::new(StakesCacheV2State {
                epoch,
                stake_history: StakeHistory::default(),
            }),
        }
    }

    pub(crate) fn empty(epoch: Epoch) -> Self {
        Self {
            stake_delegation_index: Arc::new(StakeDelegationIndex(RwLock::new(
                StakeDelegationIndexInner {
                    root_entries: RootEntries::default(),
                },
            ))),
            fork_delta: RwLock::new(ForkDelta::default()),
            state: RwLock::new(StakesCacheV2State {
                epoch,
                stake_history: StakeHistory::default(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::stakes::tests::create_stake_account, solana_pubkey::new_rand,
        solana_rent::Rent,
    };

    #[test]
    fn test_frontier_query_overlays_root_and_deltas() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);

        let root_cache = StakesCacheV2::new_from_accounts(
            [
                (
                    stake_pubkey_a,
                    StakeAccount::try_from(root_account_a).unwrap(),
                ),
                (
                    stake_pubkey_b,
                    StakeAccount::try_from(root_account_b).unwrap(),
                ),
            ]
            .into_iter(),
            0,
        );
        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        let updated_account_a = create_stake_account(30, &vote_pubkey_b, &stake_pubkey_a, &rent);
        let inserted_account_c = create_stake_account(40, &vote_pubkey_a, &stake_pubkey_c, &rent);
        fork_cache.upsert_stake_delegation(
            stake_pubkey_a,
            StakeAccount::try_from(updated_account_a).unwrap(),
        );
        fork_cache.remove_stake_delegation(&stake_pubkey_b);
        fork_cache.upsert_stake_delegation(
            stake_pubkey_c,
            StakeAccount::try_from(inserted_account_c).unwrap(),
        );

        let frontier_query = root_cache.frontier_query([&fork_cache]);
        let stake_delegations = frontier_query.iter_filtered().collect::<Vec<_>>();
        assert_eq!(stake_delegations.len(), 2);
        assert!(stake_delegations.iter().any(|(pubkey, account)| {
            **pubkey == stake_pubkey_a && account.delegation().stake == 30
        }));
        assert!(stake_delegations.iter().any(|(pubkey, account)| {
            **pubkey == stake_pubkey_c && account.delegation().stake == 40
        }));
    }

    #[test]
    fn test_frontier_query_counts_only_final_removed_root_overrides() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        let ancestor = StakesCacheV2::new_from_parent(&root_cache);
        let child = StakesCacheV2::new_from_parent(&ancestor);

        ancestor.remove_stake_delegation(&stake_pubkey);
        let updated_account = create_stake_account(20, &vote_pubkey, &stake_pubkey, &rent);
        child.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(updated_account).unwrap(),
        );

        let frontier_query = child.frontier_query([&ancestor]);
        assert_eq!(frontier_query.len_unfiltered(), 1);
        assert_eq!(frontier_query.len_filtered(), 1);
    }

    #[test]
    fn test_new_from_parent_starts_with_empty_delta() {
        let root_cache = StakesCacheV2::empty(0);
        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        assert!(fork_cache.fork_delta.read().unwrap().is_empty());

        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();
        let stake_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        fork_cache
            .upsert_stake_delegation(stake_pubkey, StakeAccount::try_from(stake_account).unwrap());

        assert!(!fork_cache.fork_delta.read().unwrap().is_empty());
    }

    #[test]
    fn test_frontier_query_yields_roots_before_frontier_inserts() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);

        let root_cache = StakesCacheV2::new_from_accounts(
            [
                (
                    stake_pubkey_a,
                    StakeAccount::try_from(root_account_a).unwrap(),
                ),
                (
                    stake_pubkey_b,
                    StakeAccount::try_from(root_account_b).unwrap(),
                ),
            ]
            .into_iter(),
            0,
        );
        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        let updated_account_a = create_stake_account(30, &vote_pubkey_b, &stake_pubkey_a, &rent);
        let inserted_account_c = create_stake_account(40, &vote_pubkey_a, &stake_pubkey_c, &rent);
        fork_cache.upsert_stake_delegation(
            stake_pubkey_a,
            StakeAccount::try_from(updated_account_a).unwrap(),
        );
        fork_cache.remove_stake_delegation(&stake_pubkey_b);
        fork_cache.upsert_stake_delegation(
            stake_pubkey_c,
            StakeAccount::try_from(inserted_account_c).unwrap(),
        );

        let frontier_query = root_cache.frontier_query([&fork_cache]);
        let query_entries = frontier_query
            .iter_filtered()
            .map(|(pubkey, stake_account)| (*pubkey, stake_account.delegation().stake))
            .collect::<Vec<_>>();
        assert_eq!(
            &query_entries,
            &[(stake_pubkey_a, 30), (stake_pubkey_c, 40)]
        );
    }

    #[test]
    fn test_root_delete_removes_entry() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);

        let root_cache = StakesCacheV2::new_from_accounts(
            [
                (
                    stake_pubkey_a,
                    StakeAccount::try_from(root_account_a).unwrap(),
                ),
                (
                    stake_pubkey_b,
                    StakeAccount::try_from(root_account_b).unwrap(),
                ),
            ]
            .into_iter(),
            0,
        );
        let index = &root_cache.stake_delegation_index;

        // Remove the stake A.
        index.apply_rooted_deltas([AHashMap::from_iter([(stake_pubkey_a, None)])]);
        {
            let inner = index.0.read().unwrap();
            assert_eq!(inner.root_entries.len(), 1);
            assert!(!inner.root_entries.contains_key(&stake_pubkey_a));
            assert!(inner.root_entries.contains_key(&stake_pubkey_b));
        }

        // Add a stake C.
        let root_account_c = create_stake_account(30, &vote_pubkey_a, &stake_pubkey_c, &rent);
        index.apply_rooted_deltas([AHashMap::from_iter([(
            stake_pubkey_c,
            Some(Arc::new(StakeAccount::try_from(root_account_c).unwrap())),
        )])]);

        let inner = index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 2);
        assert!(!inner.root_entries.contains_key(&stake_pubkey_a));
        assert!(inner.root_entries.contains_key(&stake_pubkey_b));
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey_c)
                .unwrap()
                .delegation()
                .stake,
            30
        );
    }

    #[test]
    fn test_apply_rooted_deltas_adds_new_key() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let vote_pubkey_c = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        // Create a cache with 2 entries.
        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);
        let cache = StakesCacheV2::new_from_accounts(
            [
                (
                    stake_pubkey_a,
                    StakeAccount::try_from(root_account_a).unwrap(),
                ),
                (
                    stake_pubkey_b,
                    StakeAccount::try_from(root_account_b).unwrap(),
                ),
            ]
            .into_iter(),
            0,
        );

        // Verify initial state
        let inner = cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 2);
        drop(inner);

        // Apply a delta that adds a new key.
        let new_account = create_stake_account(30, &vote_pubkey_c, &stake_pubkey_c, &rent);
        cache
            .stake_delegation_index
            .apply_rooted_deltas([AHashMap::from_iter([(
                stake_pubkey_c,
                Some(Arc::new(StakeAccount::try_from(new_account).unwrap())),
            )])]);

        let inner = cache.stake_delegation_index.0.read().unwrap();
        // All three keys should be present
        assert_eq!(inner.root_entries.len(), 3);
        assert!(inner.root_entries.contains_key(&stake_pubkey_a));
        assert!(inner.root_entries.contains_key(&stake_pubkey_b));
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey_c)
                .unwrap()
                .delegation()
                .stake,
            30
        );
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_applies_updates() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        {
            let inner = root_cache.stake_delegation_index.0.read().unwrap();
            assert_eq!(inner.root_entries.len(), 1);
            assert_eq!(
                inner
                    .root_entries
                    .get(&stake_pubkey)
                    .unwrap()
                    .delegation()
                    .stake,
                10
            );
        }

        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        let updated_account = create_stake_account(50, &vote_pubkey, &stake_pubkey, &rent);
        fork_cache.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(updated_account).unwrap(),
        );

        root_cache.apply_rooted_stake_delegation_deltas([&fork_cache]);

        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 1);
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey)
                .unwrap()
                .delegation()
                .stake,
            50
        );
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_overrides_ancestor_deltas() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        let ancestor = StakesCacheV2::new_from_parent(&root_cache);
        let grandchild = StakesCacheV2::new_from_parent(&ancestor);

        // Ancestor updates stake to 20
        let ancestor_account = create_stake_account(20, &vote_pubkey, &stake_pubkey, &rent);
        ancestor.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(ancestor_account).unwrap(),
        );

        // Grandchild updates stake to 30 (should override ancestor's 20)
        let grandchild_account = create_stake_account(30, &vote_pubkey, &stake_pubkey, &rent);
        grandchild.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(grandchild_account).unwrap(),
        );

        root_cache.apply_rooted_stake_delegation_deltas([&ancestor, &grandchild]);

        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey)
                .unwrap()
                .delegation()
                .stake,
            30
        );
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_deletes_via_none() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        // Remove the stake via None delta
        fork_cache
            .fork_delta
            .write()
            .unwrap()
            .insert(stake_pubkey, None);

        root_cache.apply_rooted_stake_delegation_deltas([&fork_cache]);

        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert!(inner.root_entries.is_empty());
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_skips_empty_deltas() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        // fork_cache has an empty fork_delta by default

        root_cache.apply_rooted_stake_delegation_deltas([&fork_cache]);

        // The rooted entry should be unchanged
        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 1);
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey)
                .unwrap()
                .delegation()
                .stake,
            10
        );
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_inserts_new_entries() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let existing_stake_pubkey = new_rand();
        let new_stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey_a, &existing_stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(
                existing_stake_pubkey,
                StakeAccount::try_from(root_account).unwrap(),
            )]
            .into_iter(),
            0,
        );

        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        let new_account = create_stake_account(20, &vote_pubkey_b, &new_stake_pubkey, &rent);
        fork_cache.upsert_stake_delegation(
            new_stake_pubkey,
            StakeAccount::try_from(new_account).unwrap(),
        );

        root_cache.apply_rooted_stake_delegation_deltas([&fork_cache]);

        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 2);
        assert!(inner.root_entries.contains_key(&existing_stake_pubkey));
        assert!(inner.root_entries.contains_key(&new_stake_pubkey));
    }

    #[test]
    fn test_apply_rooted_stake_delegation_deltas_applies_own_delta_last() {
        let rent = Rent::default();
        let vote_pubkey = new_rand();
        let stake_pubkey = new_rand();

        let root_account = create_stake_account(10, &vote_pubkey, &stake_pubkey, &rent);
        let root_cache = StakesCacheV2::new_from_accounts(
            [(stake_pubkey, StakeAccount::try_from(root_account).unwrap())].into_iter(),
            0,
        );

        let fork_cache = StakesCacheV2::new_from_parent(&root_cache);
        // Ancestor sets stake to 20.
        let ancestor_account = create_stake_account(20, &vote_pubkey, &stake_pubkey, &rent);
        fork_cache.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(ancestor_account).unwrap(),
        );

        // Root cache itself sets stake to 30 (should override ancestor).
        let root_update_account = create_stake_account(30, &vote_pubkey, &stake_pubkey, &rent);
        root_cache.upsert_stake_delegation(
            stake_pubkey,
            StakeAccount::try_from(root_update_account).unwrap(),
        );

        root_cache.apply_rooted_stake_delegation_deltas([&fork_cache]);

        let inner = root_cache.stake_delegation_index.0.read().unwrap();
        assert_eq!(
            inner
                .root_entries
                .get(&stake_pubkey)
                .unwrap()
                .delegation()
                .stake,
            30
        );
    }
}
