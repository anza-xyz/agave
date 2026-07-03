use {
    crate::{
        stake_account,
        stake_history::StakeHistory,
        stakes::{DeserializableStakes, Error},
    },
    rayon::iter::{
        IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
        IntoParallelRefMutIterator, ParallelIterator,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::utils::create_account_shared_data,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_interface::{program as stake_program, state::Delegation},
    solana_vote::vote_account::{VoteAccount, VoteAccounts, VoteAccountsHashMap},
    solana_vote_interface::state::VoteStateVersions,
    std::{
        collections::HashMap,
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
    pub(crate) fn stake_history(&self) -> &StakeHistory {
        &self.stake_history
    }
}

/// Rooted stake delegation entry.
#[derive(Clone, Debug)]
struct RootEntry {
    stake_pubkey: Pubkey,
    stake_account: Arc<StakeAccount>,
}

#[derive(Debug)]
struct StakeDelegationIndexInner {
    root_entries: Vec<Option<RootEntry>>,
    root_positions: HashMap<Pubkey, usize>,
    free_root_indices: Vec<usize>,
}

/// Index of rooted stake delegations, shared across banks.
#[derive(Debug)]
struct StakeDelegationIndex(RwLock<StakeDelegationIndexInner>);

/// Frontier-only entry that does not exist in the rooted base.
#[derive(Clone, Debug)]
struct FrontierEntry {
    stake_pubkey: Pubkey,
    stake_account: Arc<StakeAccount>,
}

/// Merged stake-delegation view for one bank frontier.
#[derive(Debug)]
pub(crate) struct FrontierQuery<'a> {
    inner: RwLockReadGuard<'a, StakeDelegationIndexInner>,
    /// Rooted slots that remain visible after applying unrooted deltas. We
    /// materialize the visible set once so repeated frontier iterations do
    /// not have to rescan all rooted entries and skip the hidden ones.
    visible_root_indices: Vec<usize>,
    overlay: HashMap<Pubkey, Option<Arc<StakeAccount>>>,
    overlay_only_inserts: Vec<FrontierEntry>,
}

impl<'a> FrontierQuery<'a> {
    pub(crate) fn len(&self) -> usize {
        self.visible_root_indices.len() + self.overlay_only_inserts.len()
    }

    pub(crate) fn par_iter(
        &'a self,
    ) -> impl IndexedParallelIterator<Item = (&'a Pubkey, &'a StakeAccount)> {
        self.visible_root_indices
            .par_iter()
            .map(move |root_index| {
                let root_entry = self.inner.root_entries[*root_index]
                    .as_ref()
                    .expect("visible rooted frontier entry must exist");
                let stake_account = self
                    .overlay
                    .get(&root_entry.stake_pubkey)
                    .and_then(|stake_account| stake_account.as_ref())
                    .map_or(root_entry.stake_account.as_ref(), Arc::as_ref);
                (&root_entry.stake_pubkey, stake_account)
            })
            .chain(
                self.overlay_only_inserts
                    .par_iter()
                    .map(|entry| (&entry.stake_pubkey, entry.stake_account.as_ref())),
            )
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl<'a> FrontierQuery<'a> {
    pub(crate) fn iter(&'a self) -> impl Iterator<Item = (&'a Pubkey, &'a StakeAccount)> {
        self.visible_root_indices
            .iter()
            .map(move |root_index| {
                let root_entry = self.inner.root_entries[*root_index]
                    .as_ref()
                    .expect("visible rooted frontier entry must exist");
                let stake_account = self
                    .overlay
                    .get(&root_entry.stake_pubkey)
                    .and_then(|stake_account| stake_account.as_ref())
                    .map_or(root_entry.stake_account.as_ref(), Arc::as_ref);
                (&root_entry.stake_pubkey, stake_account)
            })
            .chain(
                self.overlay_only_inserts
                    .iter()
                    .map(|entry| (&entry.stake_pubkey, entry.stake_account.as_ref())),
            )
    }
}

impl StakeDelegationIndex {
    fn apply_rooted_deltas(
        &self,
        deltas_in_ancestor_order: impl IntoIterator<Item = HashMap<Pubkey, Option<Arc<StakeAccount>>>>,
    ) {
        let mut inner = self.0.write().unwrap();
        for delta in deltas_in_ancestor_order {
            for (stake_pubkey, maybe_stake_account) in delta {
                match maybe_stake_account {
                    Some(stake_account) => {
                        if let Some(index) = inner.root_positions.get(&stake_pubkey).copied() {
                            inner.root_entries[index] = Some(RootEntry {
                                stake_pubkey,
                                stake_account,
                            });
                        } else {
                            let index = inner
                                .free_root_indices
                                .pop()
                                .unwrap_or_else(|| inner.root_entries.len());
                            inner.root_positions.insert(stake_pubkey, index);
                            let root_entry = Some(RootEntry {
                                stake_pubkey,
                                stake_account,
                            });
                            if index == inner.root_entries.len() {
                                inner.root_entries.push(root_entry);
                            } else {
                                inner.root_entries[index] = root_entry;
                            }
                        }
                    }
                    None => {
                        if let Some(index) = inner.root_positions.remove(&stake_pubkey) {
                            inner.root_entries[index] = None;
                            inner.free_root_indices.push(index);
                        }
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
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
pub(crate) struct StakesCacheV2 {
    stake_delegation_index: Arc<StakeDelegationIndex>,
    fork_delta: RwLock<HashMap<Pubkey, Option<Arc<StakeAccount>>>>,
    state: RwLock<StakesCacheV2State>,
}

impl StakesCacheV2 {
    fn from_root_entries_and_state(
        root_entries: Vec<Option<RootEntry>>,
        epoch: Epoch,
        stake_history: StakeHistory,
    ) -> Self {
        let mut root_positions = HashMap::with_capacity(root_entries.len());
        for (index, root_entry) in root_entries.iter().enumerate() {
            let stake_pubkey = root_entry
                .as_ref()
                .expect("stake delegation root entry must be populated")
                .stake_pubkey;
            root_positions.insert(stake_pubkey, index);
        }
        Self {
            stake_delegation_index: Arc::new(StakeDelegationIndex(RwLock::new(
                StakeDelegationIndexInner {
                    root_entries,
                    root_positions,
                    free_root_indices: Vec::new(),
                },
            ))),
            fork_delta: RwLock::new(HashMap::default()),
            state: RwLock::new(StakesCacheV2State {
                epoch,
                stake_history,
            }),
        }
    }

    pub(crate) fn new_from_accounts_for_genesis<'a, T: ReadableAccount + 'a>(
        accounts: impl IntoIterator<Item = (&'a Pubkey, &'a T)>,
    ) -> Self {
        let epoch = 0;
        let stake_history = StakeHistory::default();
        let mut vote_accounts = VoteAccountsHashMap::default();
        let mut delegated_stakes: HashMap<Pubkey, u64> = HashMap::default();
        let mut root_entries = Vec::new();

        for (pubkey, account) in accounts {
            if account.lamports() == 0 {
                continue;
            }

            if solana_vote_program::check_id(account.owner()) {
                if VoteStateVersions::is_correct_size_and_initialized(account.data()) {
                    if let Ok(vote_account) =
                        VoteAccount::try_from(create_account_shared_data(account))
                    {
                        vote_accounts.insert(*pubkey, (0, vote_account));
                    }
                }
            } else if stake_program::check_id(account.owner()) {
                if let Ok(stake_account) =
                    StakeAccount::try_from(create_account_shared_data(account))
                {
                    let delegation = stake_account.delegation();
                    #[expect(deprecated, reason = "we still use the legacy stake calculation")]
                    let stake = delegation.stake(epoch, &stake_history, None);
                    *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
                    root_entries.push(Some(RootEntry {
                        stake_pubkey: *pubkey,
                        stake_account: Arc::new(stake_account),
                    }));
                }
            }
        }

        let mut vote_accounts = VoteAccounts::from(Arc::new(vote_accounts));
        for (vote_pubkey, stake) in delegated_stakes {
            vote_accounts.add_stake(&vote_pubkey, stake);
        }

        Self::from_root_entries_and_state(root_entries, epoch, stake_history)
    }

    pub(crate) fn load_from_deserialized_delegations<F>(
        stakes: DeserializableStakes<Delegation>,
        get_account: F,
    ) -> Result<Self, Error>
    where
        F: Fn(&Pubkey) -> Option<AccountSharedData> + Sync,
    {
        let root_entries_len = stakes.stake_delegations.len();
        let mut root_entries = Vec::with_capacity(root_entries_len);
        root_entries
            .spare_capacity_mut()
            .par_iter_mut()
            .take(root_entries_len)
            .zip_eq(stakes.stake_delegations.into_par_iter())
            .try_for_each(|(root_entry, (pubkey, delegation))| {
                let Some(stake_account) = get_account(&pubkey) else {
                    return Err(Error::StakeAccountNotFound(pubkey));
                };

                let stake_account = StakeAccount::try_from(stake_account)?;
                if stake_account.delegation() == &delegation {
                    root_entry.write(Some(RootEntry {
                        stake_pubkey: pubkey,
                        stake_account: Arc::new(stake_account),
                    }));
                    Ok(())
                } else {
                    Err(Error::InvalidDelegation(pubkey))
                }
            })?;
        // SAFETY: We initialized all the `root_entries` elements up to
        // `root_entries_len`.
        unsafe {
            root_entries.set_len(root_entries_len);
        }
        Ok(Self::from_root_entries_and_state(
            root_entries,
            stakes.epoch,
            stakes.stake_history,
        ))
    }

    pub(crate) fn new_from_parent(parent: &Self) -> Self {
        let stake_delegation_index = Arc::clone(&parent.stake_delegation_index);
        let state = parent.state.read().unwrap().clone();
        Self {
            stake_delegation_index,
            fork_delta: RwLock::new(HashMap::default()),
            state: RwLock::new(state),
        }
    }

    #[cfg(test)]
    pub(crate) fn empty(epoch: Epoch) -> Self {
        Self::from_root_entries_and_state(Vec::new(), epoch, StakeHistory::default())
    }

    pub(crate) fn frontier_query<'a>(
        &self,
        caches_in_ancestor_order: impl IntoIterator<Item = &'a Self>,
    ) -> FrontierQuery<'_> {
        let mut overlay = HashMap::new();
        for cache in caches_in_ancestor_order {
            let fork_delta = cache.fork_delta.read().unwrap();
            for (stake_pubkey, stake_account) in fork_delta.iter() {
                overlay.insert(*stake_pubkey, stake_account.clone());
            }
        }
        {
            let fork_delta = self.fork_delta.read().unwrap();
            for (stake_pubkey, stake_account) in fork_delta.iter() {
                overlay.insert(*stake_pubkey, stake_account.clone());
            }
        }

        let inner = self.stake_delegation_index.0.read().unwrap();
        let mut visible_root_indices = Vec::with_capacity(inner.root_positions.len());
        for (root_index, root_entry) in inner.root_entries.iter().enumerate() {
            let Some(root_entry) = root_entry.as_ref() else {
                continue;
            };
            if matches!(overlay.get(&root_entry.stake_pubkey), Some(None)) {
                continue;
            }
            visible_root_indices.push(root_index);
        }

        let mut overlay_only_inserts = overlay
            .iter()
            .filter_map(|(stake_pubkey, maybe_stake_account)| {
                if inner.root_positions.contains_key(stake_pubkey) {
                    return None;
                }
                maybe_stake_account
                    .as_ref()
                    .map(|stake_account| FrontierEntry {
                        stake_pubkey: *stake_pubkey,
                        stake_account: Arc::clone(stake_account),
                    })
            })
            .collect::<Vec<_>>();
        overlay_only_inserts.sort_unstable_by_key(|entry| entry.stake_pubkey);

        FrontierQuery {
            inner,
            visible_root_indices,
            overlay,
            overlay_only_inserts,
        }
    }

    pub(crate) fn apply_rooted_stake_delegation_deltas<'a>(
        &self,
        caches_in_ancestor_order: impl IntoIterator<Item = &'a Self>,
    ) {
        let deltas_in_ancestor_order = caches_in_ancestor_order
            .into_iter()
            .filter_map(|cache| {
                let mut fork_delta = cache.fork_delta.write().unwrap();
                (!fork_delta.is_empty()).then(|| std::mem::take(&mut *fork_delta))
            })
            .chain({
                let mut fork_delta = self.fork_delta.write().unwrap();
                (!fork_delta.is_empty())
                    .then(|| std::mem::take(&mut *fork_delta))
                    .into_iter()
            });
        self.stake_delegation_index
            .apply_rooted_deltas(deltas_in_ancestor_order);
    }

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

    pub(crate) fn upsert_stake_delegation(&self, pubkey: Pubkey, stake_account: StakeAccount) {
        self.fork_delta
            .write()
            .unwrap()
            .insert(pubkey, Some(Arc::new(stake_account)));
    }

    pub(crate) fn remove_stake_delegation(&self, pubkey: &Pubkey) {
        self.fork_delta.write().unwrap().insert(*pubkey, None);
    }

    pub(crate) fn state(&self) -> RwLockReadGuard<'_, StakesCacheV2State> {
        self.state.read().unwrap()
    }

    pub(crate) fn activate_epoch(&self, next_epoch: Epoch, stake_history: StakeHistory) {
        let mut state = self.state.write().unwrap();
        state.epoch = next_epoch;
        state.stake_history = stake_history;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::stakes::tests::create_stake_account, rayon::ThreadPoolBuilder,
        solana_pubkey::new_rand, solana_rent::Rent,
    };

    fn root_entries_from_stake_accounts(
        stake_delegations: impl IntoIterator<Item = (Pubkey, StakeAccount)>,
    ) -> Vec<Option<RootEntry>> {
        stake_delegations
            .into_iter()
            .map(|(stake_pubkey, stake_account)| {
                Some(RootEntry {
                    stake_pubkey,
                    stake_account: Arc::new(stake_account),
                })
            })
            .collect()
    }

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

        let root_entries = root_entries_from_stake_accounts([
            (
                stake_pubkey_a,
                StakeAccount::try_from(root_account_a).unwrap(),
            ),
            (
                stake_pubkey_b,
                StakeAccount::try_from(root_account_b).unwrap(),
            ),
        ]);
        let root_cache =
            StakesCacheV2::from_root_entries_and_state(root_entries, 0, StakeHistory::default());
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

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let frontier_query = root_cache.frontier_query([&fork_cache]);
        let stake_delegations =
            thread_pool.install(|| frontier_query.par_iter().collect::<Vec<_>>());
        assert_eq!(stake_delegations.len(), 2);
        assert!(stake_delegations.iter().any(|(pubkey, account)| {
            **pubkey == stake_pubkey_a && account.delegation().stake == 30
        }));
        assert!(stake_delegations.iter().any(|(pubkey, account)| {
            **pubkey == stake_pubkey_c && account.delegation().stake == 40
        }));
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
    fn test_frontier_query_preserves_root_and_insert_order() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);

        let root_entries = root_entries_from_stake_accounts([
            (
                stake_pubkey_a,
                StakeAccount::try_from(root_account_a).unwrap(),
            ),
            (
                stake_pubkey_b,
                StakeAccount::try_from(root_account_b).unwrap(),
            ),
        ]);
        let root_cache =
            StakesCacheV2::from_root_entries_and_state(root_entries, 0, StakeHistory::default());
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

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let frontier_query = root_cache.frontier_query([&fork_cache]);
        let query_entries = thread_pool.install(|| {
            frontier_query
                .par_iter()
                .map(|(pubkey, stake_account)| (*pubkey, stake_account.delegation().stake))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            &query_entries,
            &[(stake_pubkey_a, 30), (stake_pubkey_c, 40)]
        );
    }

    #[test]
    fn test_root_slot_reuse_after_delete() {
        let rent = Rent::default();
        let vote_pubkey_a = new_rand();
        let vote_pubkey_b = new_rand();
        let stake_pubkey_a = new_rand();
        let stake_pubkey_b = new_rand();
        let stake_pubkey_c = new_rand();

        let root_account_a = create_stake_account(10, &vote_pubkey_a, &stake_pubkey_a, &rent);
        let root_account_b = create_stake_account(20, &vote_pubkey_b, &stake_pubkey_b, &rent);

        let root_entries = root_entries_from_stake_accounts([
            (
                stake_pubkey_a,
                StakeAccount::try_from(root_account_a).unwrap(),
            ),
            (
                stake_pubkey_b,
                StakeAccount::try_from(root_account_b).unwrap(),
            ),
        ]);
        let root_cache =
            StakesCacheV2::from_root_entries_and_state(root_entries, 0, StakeHistory::default());
        let index = &root_cache.stake_delegation_index;
        let removed_index = {
            let inner = index.0.read().unwrap();
            *inner.root_positions.get(&stake_pubkey_a).unwrap()
        };

        index.apply_rooted_deltas([HashMap::from_iter([(stake_pubkey_a, None)])]);
        {
            let inner = index.0.read().unwrap();
            assert_eq!(inner.root_entries.len(), 2);
            assert_eq!(&inner.free_root_indices, &[removed_index]);
        }

        let root_account_c = create_stake_account(30, &vote_pubkey_a, &stake_pubkey_c, &rent);
        index.apply_rooted_deltas([HashMap::from_iter([(
            stake_pubkey_c,
            Some(Arc::new(StakeAccount::try_from(root_account_c).unwrap())),
        )])]);

        let inner = index.0.read().unwrap();
        assert_eq!(inner.root_entries.len(), 2);
        assert!(inner.free_root_indices.is_empty());
        assert_eq!(
            inner.root_positions.get(&stake_pubkey_c),
            Some(&removed_index)
        );
        assert_eq!(
            inner.root_entries[removed_index]
                .as_ref()
                .map(|entry| entry.stake_pubkey),
            Some(stake_pubkey_c)
        );
    }
}
