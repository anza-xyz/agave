//! Stakes serve as a cache of stake and vote accounts to derive
//! node stakes
#[cfg(feature = "frozen-abi")]
use solana_frozen_abi::stable_abi::{context::SequenceLenMax, sample_collection_sized};
use {
    crate::{
        alpenglow_epoch_type::RewardEpochDelegatedStakes,
        stake_account,
        stake_delegation::{delegation_activation_status, delegation_effective_stake},
        stake_history::StakeHistory,
    },
    ahash::RandomState as AHashRandomState,
    imbl::{
        GenericHashMap as ImblGenericHashMap, HashMap as ImblHashMap, HashSet as ImblHashSet,
        shared_ptr::DefaultSharedPtr,
    },
    indexmap::IndexMap,
    log::error,
    num_derive::ToPrimitive,
    rayon::{ThreadPool, prelude::*},
    serde::Serialize,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::utils::create_account_shared_data,
    solana_clock::Epoch,
    solana_leader_schedule::SlotLeader,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_pubkey::Pubkey,
    solana_stake_interface::{
        program as stake_program,
        state::{Delegation, StakeActivationStatus},
    },
    solana_vote::vote_account::{VoteAccount, VoteAccounts, VoteAccountsHashMap},
    solana_vote_interface::state::VoteStateVersions,
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        iter::Enumerate,
        sync::{Arc, RwLock, RwLockReadGuard},
    },
    thiserror::Error,
    wincode::SchemaWrite,
};
#[cfg(feature = "dev-context-only-utils")]
use {
    qualifier_attr::{field_qualifiers, qualifiers},
    solana_stake_interface::state::Stake,
    std::collections::BTreeMap,
};

mod serde_stakes;
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) use serde_stakes::DeserializableDelegationStakes;
pub use serde_stakes::SerdeStakesToStakeFormat;
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) use serde_stakes::serialize_stake_accounts_to_delegation_format;

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
pub(crate) type DelegatedStakes = ImblHashMap<Pubkey, u64>;

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
        use_fixed_point_stake_math: bool,
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
                stakes.remove_stake_delegation(
                    pubkey,
                    new_rate_activation_epoch,
                    use_fixed_point_stake_math,
                );
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
                            stakes.upsert_vote_account(pubkey, vote_account)
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
                        use_fixed_point_stake_math,
                    );
                }
                Err(_) => {
                    let mut stakes = self.0.write().unwrap();
                    stakes.remove_stake_delegation(
                        pubkey,
                        new_rate_activation_epoch,
                        use_fixed_point_stake_math,
                    );
                }
            }
        }
    }

    pub(crate) fn activate_epoch(
        &self,
        next_epoch: Epoch,
        stake_history: StakeHistory,
        vote_accounts: VoteAccounts,
        delegated_stakes: DelegatedStakes,
    ) {
        let mut stakes = self.0.write().unwrap();
        stakes.activate_epoch(next_epoch, stake_history, vote_accounts, delegated_stakes)
    }

    pub(crate) fn refresh_delegated_stakes(
        &self,
        new_rate_activation_epoch: Option<Epoch>,
        use_fixed_point_stake_math: bool,
    ) {
        let mut stakes = self.0.write().unwrap();
        stakes.refresh_delegated_stakes(new_rate_activation_epoch, use_fixed_point_stake_math);
    }

    pub(crate) fn update_stake_delegations_snapshot(&self) {
        self.0.write().unwrap().update_stake_delegations_snapshot();
    }

    #[cfg(test)]
    pub(crate) fn stake_delegation_counts(&self) -> (usize, usize, usize) {
        let stakes = self.0.read().unwrap();
        let StakeDelegationsMaps {
            snapshot,
            overrides: _,
            additions,
            removals,
        } = &stakes.stake_delegations;
        (snapshot.len(), additions.len(), removals.len())
    }
}

pub(crate) type StakeDelegationsSnapshot<T> = IndexMap<Pubkey, T, AHashRandomState>;

/// Indexed overlay view of stake delegations.
///
/// Unrooted delegations override rooted delegations with the same pubkey. Unrooted delegations
/// without a rooted counterpart are appended after the rooted entries, and unrooted removals hide
/// rooted entries until the fork is squashed.
pub(crate) struct IndexedStakeDelegations<'a, T: Clone> {
    snapshot: Arc<StakeDelegationsSnapshot<T>>,
    overrides: HashMap<&'a usize, &'a T, BuildNoHashHasher<usize>>,
    additions: Vec<(&'a Pubkey, &'a T)>,
    removals: HashSet<&'a Pubkey>,
}

impl<'a, T: Clone + Sync> IndexedStakeDelegations<'a, T> {
    pub(crate) fn len_unfiltered(&self) -> usize {
        let Self {
            snapshot,
            additions,
            ..
        } = self;
        snapshot.len().wrapping_add(additions.len())
    }

    /// Pending rooted removals yield `None` so the iterator remains indexed.
    pub(crate) fn par_iter_unfiltered(
        &self,
    ) -> impl IndexedParallelIterator<Item = Option<(&Pubkey, &T)>> {
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
            ..
        } = self;
        snapshot
            .par_iter()
            .enumerate()
            .map(move |(index, (pubkey, snapshot_stake_delegation))| {
                (!removals.contains(pubkey)).then(|| {
                    let stake_delegation = overrides
                        .get(&index)
                        .map(|&stake_delegation| stake_delegation)
                        .unwrap_or(snapshot_stake_delegation);
                    (pubkey, stake_delegation)
                })
            })
            .chain(
                additions
                    .par_iter()
                    .map(|&(pubkey, stake_delegation)| Some((pubkey, stake_delegation))),
            )
    }

    pub(crate) fn par_iter_filtered(&self) -> impl ParallelIterator<Item = (&Pubkey, &T)> {
        self.par_iter_unfiltered().flatten()
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct StakeDelegationsIter<'a, T: Clone> {
    stake_delegations_snapshot: Enumerate<indexmap::map::Iter<'a, Pubkey, T>>,
    stake_delegations_overrides:
        &'a ImblGenericHashMap<usize, T, BuildNoHashHasher<usize>, DefaultSharedPtr>,
    stake_delegations_additions:
        imbl::hashmap::Iter<'a, Pubkey, T, imbl::shared_ptr::DefaultSharedPtr>,
    stake_delegations_removals: &'a ImblHashSet<Pubkey>,
}

impl<'a, T: Clone> Iterator for StakeDelegationsIter<'a, T> {
    type Item = (&'a Pubkey, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        let Self {
            stake_delegations_snapshot,
            stake_delegations_overrides,
            stake_delegations_additions,
            stake_delegations_removals,
        } = self;
        let stake_delegation = loop {
            if let Some((index, (pubkey, snapshot_stake_delegation))) =
                stake_delegations_snapshot.next()
            {
                if stake_delegations_removals.contains(&pubkey) {
                    continue;
                }
                let stake_delegation = stake_delegations_overrides
                    .get(&index)
                    .unwrap_or(snapshot_stake_delegation);
                break (pubkey, stake_delegation);
            } else if let Some((pubkey, stake_delegation)) = stake_delegations_additions.next() {
                break (pubkey, stake_delegation);
            } else {
                return None;
            }
        };
        Some(stake_delegation)
    }
}

impl<'a, T: Clone> ExactSizeIterator for StakeDelegationsIter<'a, T> {
    fn len(&self) -> usize {
        let Self {
            stake_delegations_snapshot,
            stake_delegations_additions,
            stake_delegations_removals,
            ..
        } = self;
        stake_delegations_snapshot
            .len()
            .wrapping_add(stake_delegations_additions.len())
            .wrapping_add(stake_delegations_removals.len())
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct StakeDelegationsMaps<T: Clone> {
    /// Snapshot of stake delegations, that can be updated with new entries
    /// from `stake_delegations_overrides`, `stake_delegations_additions`, and
    /// `stake_delegations_removals` via [`Self::update_stake_delegations_snapshot`].
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "Arc::new(sample_collection_sized(rng, SequenceLenMax(1)))")
    )]
    snapshot: Arc<StakeDelegationsSnapshot<T>>,

    /// Overrides of stake delegations from `stake_delegations_snapshot`, that
    /// come from committed transactions. They can be applied to the snapshot
    /// via [`Self::update_stake_delegations_snapshot`].
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_collection_sized(rng, SequenceLenMax(1))")
    )]
    overrides: ImblGenericHashMap<usize, T, BuildNoHashHasher<usize>, DefaultSharedPtr>,

    /// Additions of stake delegations that are not part of
    /// `stake_delegations_snapshot`, that come from commited transactions.
    /// They can be applied to the snapshot via
    /// [`Self::update_stake_delegations_snapshot`].
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_collection_sized(rng, SequenceLenMax(1))")
    )]
    additions: ImblHashMap<Pubkey, T>,

    /// Stale delegation removals of `stake_delegations_snapshot` elements,
    /// that come from commited transactions. The removals are applied with
    /// [`Self::update_stake_delegations_snapshot`].
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_collection_sized(rng, SequenceLenMax(1))")
    )]
    removals: ImblHashSet<Pubkey>,
}

impl<T: Clone> Default for StakeDelegationsMaps<T> {
    fn default() -> Self {
        Self {
            snapshot: Arc::new(StakeDelegationsSnapshot::default()),
            overrides: ImblGenericHashMap::default(),
            additions: ImblHashMap::default(),
            removals: ImblHashSet::default(),
        }
    }
}

impl<'a, T: Clone> StakeDelegationsMaps<T> {
    fn update_snapshot(&mut self) {
        if self.is_empty() {
            return;
        }
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
        } = self;
        let snapshot = Arc::make_mut(snapshot);
        for (ix, new_stake_delegation) in std::mem::take(overrides) {
            let (_, old_stake_delegation) = snapshot.get_index_mut(ix)
                .unwrap_or_else(|| panic!(
                    "index `{ix}` present in `stake_delegation_overrides` does not exist in `stake_delegations_snapshot`"));
            *old_stake_delegation = new_stake_delegation;
        }
        for pubkey in std::mem::take(removals) {
            snapshot.swap_remove(&pubkey);
        }
        for (pubkey, stake_delegation) in std::mem::take(additions) {
            snapshot.insert(pubkey, stake_delegation);
        }
    }

    pub(crate) fn get(&self, pubkey: &Pubkey) -> Option<&T> {
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
        } = self;
        additions.get(pubkey).or_else(|| {
            if let Some((index, _pubkey, snapshot_stake_delegation)) = snapshot.get_full(pubkey) {
                if removals.contains(pubkey) {
                    None
                } else {
                    Some(overrides.get(&index).unwrap_or(snapshot_stake_delegation))
                }
            } else {
                None
            }
        })
    }

    pub(crate) fn len(&self) -> usize {
        let Self {
            snapshot,
            additions,
            removals,
            ..
        } = self;
        snapshot
            .len()
            .wrapping_add(additions.len())
            .wrapping_sub(removals.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
        } = self;
        snapshot.is_empty() && overrides.is_empty() && additions.is_empty() && removals.is_empty()
    }

    /// Returns an iterator of stake delegations.
    ///
    /// # Performance
    ///
    /// Stake delegations consist of a snapshot represented by `[IndexMap]` and
    /// overlay changes represented by `[imbl::HashMap]`s. `[imbl::HashMap]` is
    /// a [hash array mapped trie (HAMT)][hamt], which means that inserts,
    /// deletions and lookups are average-case O(1) and worst-case O(log n).
    /// However, the performance of iterations is poor due to depth-first
    /// traversal and jumps. Currently it's also impossible to iterate over it
    /// with [`rayon`].
    ///
    /// This method is a good choice if the caller iterates over stake
    /// delegations only once. If there more subsequent iterations needed,
    /// use [`StakeDelegationsMaps::indexed`].
    ///
    /// [hamt]: https://en.wikipedia.org/wiki/Hash_array_mapped_trie
    pub(crate) fn iter(&'a self) -> StakeDelegationsIter<'a, T> {
        self.into_iter()
    }

    /// Collects stake delegations into an [`IndexedStakeDelegations`],
    /// which then can be used for multiple subsequent iterations and/or
    /// indexed parallel iteration with [`rayon`].
    ///
    /// # Performance
    ///
    /// The execution of this method collects elements of multiple [`imbl::HashMap`]
    /// fields, which are [hash array mapped tries (HAMT)][hamt], so that
    /// operation involves a depth-first traversal with jumps. However, it's
    /// still a reasonable tradeoff if the caller iterates over these elements
    /// multiple times or wants to use [`rayon`].
    ///
    /// [hamt]: https://en.wikipedia.org/wiki/Hash_array_mapped_trie
    pub(crate) fn indexed(&'a self) -> IndexedStakeDelegations<'a, T> {
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
            ..
        } = self;
        let snapshot = Arc::clone(snapshot);
        // Getting a value from `ImblHashMap` is an O(log n) operation. Given
        // that we have to perform a lookup for every element from
        // `stake_delegations_snapshot`, that would slow down every iteration.
        // To avoid that, collect the values into a regular `HashMap`. The
        // tradeoff of collecting once for faster iterations.
        let overrides = overrides.iter().collect();
        // Iterating over `ImblHashMap` is also slow, and involves a
        // depth-first traversal with jumps. Similarly, collect it once, so
        // then iteration becomes faster.
        let additions = additions.iter().collect();
        let removals = removals.iter().collect();
        IndexedStakeDelegations {
            snapshot,
            overrides,
            additions,
            removals,
        }
    }
}

impl<'a> StakeDelegationsMaps<StakeAccount> {
    fn upsert_stake_delegation(
        &'a mut self,
        stake_pubkey: Pubkey,
        stake_account: StakeAccount,
    ) -> Option<Cow<'a, StakeAccount>> {
        let Self {
            snapshot,
            overrides,
            additions,
            removals,
        } = self;
        match snapshot.get_full(&stake_pubkey) {
            Some((index, _pubkey, snapshot_stake_account)) => {
                removals.remove(&stake_pubkey);
                // Insert new `stake_account` to the overrides.
                // After insertion, we need to subtract the old stake - which
                // might be represented either by an already existing override
                // entry, or the snapshot entry.
                let old_stake_account = overrides
                    .insert(index, stake_account)
                    .map(Cow::Owned)
                    .unwrap_or_else(|| Cow::Borrowed(snapshot_stake_account));
                Some(old_stake_account)
            }
            None => {
                // Check if stake exists in additions by removing and re-inserting
                // This is necessary because ImblHashMap doesn't provide a get+insert atomically
                if let Some(old_stake_account) = additions.remove(&stake_pubkey) {
                    // Stake already existed - re-insert new account and return old
                    // old_stake_account is already owned, just return it wrapped in Cow
                    additions.insert(stake_pubkey, stake_account);
                    Some(Cow::Owned(old_stake_account))
                } else {
                    // New stake - just insert
                    additions.insert(stake_pubkey, stake_account);
                    None
                }
            }
        }
    }
}

impl<'a, T: Clone> IntoIterator for &'a StakeDelegationsMaps<T> {
    type Item = <Self::IntoIter as Iterator>::Item;
    type IntoIter = StakeDelegationsIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        let StakeDelegationsMaps {
            snapshot: stake_delegations_snapshot,
            overrides: stake_delegations_overrides,
            additions: stake_delegations_additions,
            removals: stake_delegations_removals,
        } = self;
        let stake_delegations_snapshot = stake_delegations_snapshot.iter().enumerate();
        let stake_delegations_additions = stake_delegations_additions.iter();
        StakeDelegationsIter {
            stake_delegations_snapshot,
            stake_delegations_overrides,
            stake_delegations_additions,
            stake_delegations_removals,
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl<T: Clone + PartialEq> PartialEq for StakeDelegationsMaps<T> {
    fn eq(&self, other: &Self) -> bool {
        // Collect into `BTreeMap` to ensure the same order.
        let self_stakes: BTreeMap<_, _> = self.iter().collect();
        let other_stakes: BTreeMap<_, _> = other.iter().collect();
        self_stakes.eq(&other_stakes)
    }
}

mod wincode_compat {
    use {
        std::borrow::Borrow,
        wincode::{
            SchemaWrite, WriteResult, config::Config, containers::FromIntoIterator, len::BincodeLen,
        },
    };

    use super::{Pubkey, StakeDelegationsIter, StakeDelegationsMaps};

    // A wrapper bridging `StakeDelegationsMaps` with wincode.
    //
    // Wincode's `containers::FromIntoIterator` has the following trait bounds
    // for the wrapped type `Coll`:
    //
    // - `Coll: IntoIterator`
    // - `for<'a> &'a Coll: IntoIterator<Item: SchemaWrite<C>, IntoIter: ExactSizeIterator>`
    //
    // Which practically means that `Coll` needs the following `impl` blocks:
    //
    // - `impl IntoIterator for Coll`
    // - `impl IntoIterator for &'a Coll`
    //
    // Implementing the first one for `StakeDelegationsMaps<T>` is difficult,
    // because:
    //
    // - We can't move out of the `Arc<StakeDelegationsSnapshot<T>>` to yield
    //   owned `(Pubkey, T)` pair from it.
    // - Yielding references is not possible, since `self` and all its
    //   non-`Arc` fields would be owned inside the `into_iter()` method and we
    //   can't return references to the local values.
    //
    // It would be great if `FromIntoIterator` worked for types having only
    // `impl IntoIterator for &'a Coll`, but until that happens, we need to
    // use this workaround.
    pub(crate) struct StakeDelegationsMapsRef<'a, T: Clone>(&'a StakeDelegationsMaps<T>);

    impl<'a, T: Clone> IntoIterator for StakeDelegationsMapsRef<'a, T> {
        type Item = <Self::IntoIter as Iterator>::Item;
        type IntoIter = StakeDelegationsIter<'a, T>;

        fn into_iter(self) -> Self::IntoIter {
            self.0.into_iter()
        }
    }

    impl<'a, T: Clone> IntoIterator for &StakeDelegationsMapsRef<'a, T> {
        type Item = <Self::IntoIter as Iterator>::Item;
        type IntoIter = StakeDelegationsIter<'a, T>;

        fn into_iter(self) -> Self::IntoIter {
            self.0.into_iter()
        }
    }

    unsafe impl<C: Config, T: Clone> SchemaWrite<C> for StakeDelegationsMaps<T>
    where
        C: Config,
        T: Clone + SchemaWrite<C>,
        for<'a> (&'a Pubkey, &'a T): Borrow<(&'a Pubkey, &'a <T as SchemaWrite<C>>::Src)>,
    {
        type Src = Self;

        fn size_of(src: &Self::Src) -> WriteResult<usize> {
            let stake_delegations = StakeDelegationsMapsRef(&src);
            <FromIntoIterator<StakeDelegationsMapsRef<'_, T>, BincodeLen> as SchemaWrite<C>>::size_of(
                &stake_delegations,
            )
        }

        fn write(writer: impl wincode::io::Writer, src: &Self::Src) -> WriteResult<()> {
            let stake_delegations = StakeDelegationsMapsRef(&src);
            <FromIntoIterator<StakeDelegationsMapsRef<'_, T>, BincodeLen> as SchemaWrite<C>>::write(
                writer,
                &stake_delegations,
            )
        }
    }
}

/// The generic type T is either Delegation or StakeAccount.
/// [`Stakes<Delegation>`] is equivalent to the old code and is used for backward
/// compatibility in [`crate::bank::BankFieldsToDeserialize`].
/// But banks cache [`Stakes<StakeAccount>`] which includes the entire stake
/// account and StakeStateV2 deserialized from the account. Doing so, will remove
/// the need to load the stake account from accounts-db when working with
/// stake-delegations.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Default, Clone, Debug, Serialize, SchemaWrite)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(
        vote_accounts(pub),
        stake_delegations(pub),
        delegated_stakes(pub),
        unused(pub),
        epoch(pub),
        stake_history(pub),
    )
)]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct Stakes<T: Clone> {
    /// vote accounts
    vote_accounts: VoteAccounts,

    /// stake_delegations
    stake_delegations: StakeDelegationsMaps<T>,

    /// current effective stake delegated to each vote account pubkey
    #[cfg_attr(feature = "frozen-abi", stable_abi_sample(with = "Default::default()"))]
    #[serde(skip)]
    #[wincode(skip)]
    delegated_stakes: DelegatedStakes,

    /// unused
    unused: u64,

    /// current epoch, used to calculate current stake
    epoch: Epoch,

    /// history of staking levels
    stake_history: StakeHistory,
}

impl<T: Clone> Stakes<T> {
    pub fn new(vote_accounts: VoteAccounts, epoch: Epoch) -> Stakes<T> {
        Stakes {
            vote_accounts,
            epoch,
            stake_delegations: StakeDelegationsMaps::default(),
            delegated_stakes: DelegatedStakes::default(),
            unused: 0,
            stake_history: StakeHistory::default(),
        }
    }

    pub fn clone_and_filter_for_vat(
        &self,
        max_vote_accounts: usize,
        minimum_vote_account_balance: u64,
    ) -> Stakes<T> {
        Self::new(
            self.vote_accounts
                .clone_and_filter_for_vat(max_vote_accounts, minimum_vote_account_balance),
            self.epoch,
        )
    }

    pub fn vote_accounts(&self) -> &VoteAccounts {
        &self.vote_accounts
    }

    pub(crate) fn staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        self.vote_accounts.staked_nodes()
    }

    fn update_stake_delegations_snapshot(&mut self) {
        self.stake_delegations.update_snapshot();
    }

    /// Destructure self and return the fields needed by EpochStakes
    pub(crate) fn into_epoch_stakes_fields(self) -> (Epoch, VoteAccounts, StakeHistory) {
        let Self {
            vote_accounts,
            stake_delegations: _,
            delegated_stakes: _,
            unused: _,
            epoch,
            stake_history,
        } = self;
        (epoch, vote_accounts, stake_history)
    }
}

impl Stakes<StakeAccount> {
    pub(crate) fn new_from_accounts_for_genesis<'a, T: ReadableAccount + 'a>(
        new_rate_activation_epoch: Option<Epoch>,
        accounts: impl IntoIterator<Item = (&'a Pubkey, &'a T)>,
        use_fixed_point_stake_math: bool,
    ) -> Self {
        let stake_history = StakeHistory::default();
        let mut vote_accounts = VoteAccountsHashMap::default();
        let mut delegated_stakes = DelegatedStakes::default();
        let mut snapshot = IndexMap::default();
        let epoch = 0;

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
                let stake = delegation_effective_stake(
                    delegation,
                    epoch,
                    &stake_history,
                    new_rate_activation_epoch,
                    use_fixed_point_stake_math,
                );
                if stake != 0 {
                    *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
                }
                snapshot.insert(*pubkey, stake_account);
            }
        }

        let mut vote_accounts = VoteAccounts::from(Arc::new(vote_accounts));
        for (vote_pubkey, stake) in &delegated_stakes {
            vote_accounts.add_stake(vote_pubkey, *stake);
        }

        let snapshot = Arc::new(snapshot);

        Self {
            vote_accounts,
            stake_delegations: StakeDelegationsMaps {
                snapshot,
                overrides: ImblGenericHashMap::new(),
                additions: ImblHashMap::new(),
                removals: ImblHashSet::new(),
            },
            delegated_stakes,
            unused: 0,
            epoch,
            stake_history,
        }
    }

    /// Creates a Stake<StakeAccount> from DeserializableDelegationStakes by loading the
    /// full account state for respective stake pubkeys. get_account function
    /// should return the account at the respective slot where stakes where
    /// cached.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn load_from_deserialized_delegations<F>(
        stakes: DeserializableDelegationStakes,
        get_account: F,
    ) -> Result<Self, Error>
    where
        F: Fn(&Pubkey) -> Option<AccountSharedData> + Sync,
    {
        let snapshot = stakes
            .stake_delegations
            .into_par_iter()
            // We use fold/reduce to aggregate the results, which does a bit more work than calling
            // collect()/collect_vec_list(), then IndexMap::from_iter(collected.into_iter()) and
            // imbl::HashMap::from_iter(collected.into_iter()), but it does it in background threads,
            // so effectively it's faster.
            .try_fold(IndexMap::default, |mut map, (pubkey, delegation)| {
                let Some(stake_account) = get_account(&pubkey) else {
                    return Err(Error::StakeAccountNotFound(pubkey));
                };

                // Assert that all valid vote-accounts referenced in stake delegations are already
                // contained in `stakes.vote_account`.
                let voter_pubkey = &delegation.voter_pubkey;
                if stakes.vote_accounts.get(voter_pubkey).is_none()
                    && let Some(account) = get_account(voter_pubkey)
                    && VoteStateVersions::is_correct_size_and_initialized(account.data())
                    && VoteAccount::try_from(account.clone()).is_ok()
                {
                    error!("vote account not cached: {voter_pubkey}, {account:?}");
                    return Err(Error::VoteAccountNotCached(*voter_pubkey));
                }

                let stake_account = StakeAccount::try_from(stake_account)?;
                // Sanity check that the delegation is consistent with what is
                // stored in the account.
                if stake_account.delegation() == &delegation {
                    map.insert(pubkey, stake_account);
                    Ok(map)
                } else {
                    Err(Error::InvalidDelegation(pubkey))
                }
            })
            .try_reduce(IndexMap::default, |mut a, b| {
                a.extend(b);
                Ok(a)
            })?;
        let snapshot = Arc::new(snapshot);

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
            stake_delegations: StakeDelegationsMaps {
                snapshot,
                overrides: ImblGenericHashMap::new(),
                additions: ImblHashMap::new(),
                removals: ImblHashSet::new(),
            },
            delegated_stakes: DelegatedStakes::default(),
            unused: stakes.unused,
            epoch: stakes.epoch,
            stake_history: stakes.stake_history,
        })
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        epoch: Epoch,
        vote_accounts: VoteAccounts,
        snapshot: Arc<StakeDelegationsSnapshot<StakeAccount>>,
    ) -> Self {
        let stake_history = StakeHistory::default();
        let delegated_stakes =
            Self::calculate_delegated_stakes(snapshot.values(), epoch, &stake_history, None, true);
        Self {
            vote_accounts,
            stake_delegations: StakeDelegationsMaps {
                snapshot,
                overrides: ImblGenericHashMap::new(),
                additions: ImblHashMap::new(),
                removals: ImblHashSet::new(),
            },
            delegated_stakes,
            unused: 0,
            epoch,
            stake_history,
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
        stake_delegations: &IndexedStakeDelegations<'_, StakeAccount>,
        use_fixed_point_stake_math: bool,
    ) -> (
        StakeHistory,
        VoteAccounts,
        DelegatedStakes,
        RewardEpochDelegatedStakes,
    ) {
        // Wrap up the prev epoch by adding new stake history entry for the
        // prev epoch.
        let (stake_history_entry, effective_delegated_stakes) = thread_pool.install(|| {
            stake_delegations
                .par_iter_filtered()
                .fold(
                    || (StakeActivationStatus::default(), HashMap::default()),
                    |(acc, mut delegated_stakes), (_stake_pubkey, stake_account)| {
                        let delegation = stake_account.delegation();
                        let activation_status = delegation_activation_status(
                            delegation,
                            self.epoch,
                            &self.stake_history,
                            new_rate_activation_epoch,
                            use_fixed_point_stake_math,
                        );
                        *delegated_stakes.entry(delegation.voter_pubkey).or_default() +=
                            activation_status.effective;
                        (acc + activation_status, delegated_stakes)
                    },
                )
                .reduce(
                    || (StakeActivationStatus::default(), HashMap::default()),
                    |(activation_status_a, delegated_stakes_a),
                     (activation_status_b, delegated_stakes_b)| {
                        (
                            activation_status_a + activation_status_b,
                            merge_delegated_stakes(delegated_stakes_a, delegated_stakes_b),
                        )
                    },
                )
        });
        let mut stake_history = self.stake_history.clone();
        stake_history.add(self.epoch, stake_history_entry);
        // Refresh the stake distribution of vote accounts for the next epoch,
        // using new stake history.
        let (vote_accounts, delegated_stakes) = refresh_vote_accounts(
            thread_pool,
            next_epoch,
            &self.vote_accounts,
            stake_delegations,
            &stake_history,
            new_rate_activation_epoch,
            use_fixed_point_stake_math,
        );
        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: self.epoch,
            delegated_stakes: effective_delegated_stakes,
        };
        (
            stake_history,
            vote_accounts,
            delegated_stakes,
            reward_epoch_delegated_stakes,
        )
    }

    pub(crate) fn activate_epoch(
        &mut self,
        next_epoch: Epoch,
        stake_history: StakeHistory,
        vote_accounts: VoteAccounts,
        delegated_stakes: DelegatedStakes,
    ) {
        self.epoch = next_epoch;
        self.stake_history = stake_history;
        self.vote_accounts = vote_accounts;
        self.delegated_stakes = delegated_stakes;
    }

    fn calculate_delegated_stakes<'a>(
        stake_delegations: impl IntoIterator<Item = &'a StakeAccount>,
        epoch: Epoch,
        stake_history: &StakeHistory,
        new_rate_activation_epoch: Option<Epoch>,
        use_fixed_point_stake_math: bool,
    ) -> DelegatedStakes {
        let mut delegated_stakes = DelegatedStakes::new();
        for stake_account in stake_delegations {
            let delegation = stake_account.delegation();
            let stake = delegation_effective_stake(
                delegation,
                epoch,
                stake_history,
                new_rate_activation_epoch,
                use_fixed_point_stake_math,
            );
            if stake != 0 {
                *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
            }
        }
        delegated_stakes
    }

    fn refresh_delegated_stakes(
        &mut self,
        new_rate_activation_epoch: Option<Epoch>,
        use_fixed_point_stake_math: bool,
    ) {
        let stake_delegations = self.stake_delegations().iter();
        let delegated_stakes = Self::calculate_delegated_stakes(
            stake_delegations.map(|(_pubkey, stake_account)| stake_account),
            self.epoch,
            &self.stake_history,
            new_rate_activation_epoch,
            use_fixed_point_stake_math,
        );
        self.delegated_stakes = delegated_stakes;
    }

    fn add_delegated_stake(&mut self, voter_pubkey: Pubkey, stake: u64) {
        if stake == 0 {
            return;
        }
        *self.delegated_stakes.entry(voter_pubkey).or_default() += stake;
    }

    fn sub_delegated_stake(&mut self, voter_pubkey: &Pubkey, stake: u64) {
        if stake == 0 {
            return;
        }
        let current_stake = self
            .delegated_stakes
            .get_mut(voter_pubkey)
            .expect("subtraction from missing delegated stake");
        *current_stake = current_stake
            .checked_sub(stake)
            .expect("subtraction value exceeds delegated stake");
        if *current_stake == 0 {
            self.delegated_stakes.remove(voter_pubkey);
        }
    }

    fn remove_vote_account(&mut self, vote_pubkey: &Pubkey) -> Option<VoteAccount> {
        self.vote_accounts.remove(vote_pubkey).map(|(_, a)| a)
    }

    fn remove_stake_delegation(
        &mut self,
        stake_pubkey: &Pubkey,
        new_rate_activation_epoch: Option<Epoch>,
        use_fixed_point_stake_math: bool,
    ) {
        let Self {
            stake_delegations, ..
        } = self;
        let StakeDelegationsMaps {
            snapshot,
            overrides,
            additions,
            removals,
        } = stake_delegations;
        let (voter_pubkey, stake) = if let Some(stake_account) = additions.remove(stake_pubkey) {
            // The stake delegation we're removing is added in
            // `stake_delegations_additions`.
            let delegation = stake_account.delegation();
            let voter_pubkey = delegation.voter_pubkey;
            let stake = delegation_effective_stake(
                delegation,
                self.epoch,
                &self.stake_history,
                new_rate_activation_epoch,
                use_fixed_point_stake_math,
            );
            (voter_pubkey, stake)
        } else if let Some((ix, _, stake_account)) = snapshot.get_full(stake_pubkey) {
            // The stake delegation we're removing is present in
            // `stake_delegations_snapshot`, schedule its removal.
            removals.insert(*stake_pubkey);
            // Check whether it was updated in `stake_delegation_overrides`.
            // If yes, use the up-to-date value for calculating the
            // effective stake.
            let stake_account = overrides
                .remove(&ix)
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed(stake_account));

            let delegation = stake_account.delegation();
            let voter_pubkey = delegation.voter_pubkey;
            let stake = delegation_effective_stake(
                delegation,
                self.epoch,
                &self.stake_history,
                new_rate_activation_epoch,
                use_fixed_point_stake_math,
            );
            (voter_pubkey, stake)
        } else {
            return;
        };
        self.sub_delegated_stake(&voter_pubkey, stake);
        self.vote_accounts.sub_stake(&voter_pubkey, stake);
    }

    fn upsert_vote_account(
        &mut self,
        vote_pubkey: &Pubkey,
        vote_account: VoteAccount,
    ) -> Option<VoteAccount> {
        debug_assert_ne!(vote_account.lamports(), 0u64);

        let calculate_delegated_stake = || {
            self.delegated_stakes
                .get(vote_pubkey)
                .copied()
                .unwrap_or_default()
        };
        self.vote_accounts
            .insert(*vote_pubkey, vote_account, calculate_delegated_stake)
    }

    fn upsert_stake_delegation(
        &mut self,
        stake_pubkey: Pubkey,
        stake_account: StakeAccount,
        new_rate_activation_epoch: Option<Epoch>,
        use_fixed_point_stake_math: bool,
    ) {
        debug_assert_ne!(stake_account.lamports(), 0u64);
        let delegation = stake_account.delegation();
        let voter_pubkey = delegation.voter_pubkey;
        let stake = delegation_effective_stake(
            delegation,
            self.epoch,
            &self.stake_history,
            new_rate_activation_epoch,
            use_fixed_point_stake_math,
        );
        if let Some(old_stake_account) = self
            .stake_delegations
            .upsert_stake_delegation(stake_pubkey, stake_account)
        {
            let old_delegation = old_stake_account.delegation();
            let old_voter_pubkey = old_delegation.voter_pubkey;
            let old_stake = delegation_effective_stake(
                old_delegation,
                self.epoch,
                &self.stake_history,
                new_rate_activation_epoch,
                use_fixed_point_stake_math,
            );
            if voter_pubkey != old_voter_pubkey || stake != old_stake {
                self.sub_delegated_stake(&old_voter_pubkey, old_stake);
                self.add_delegated_stake(voter_pubkey, stake);
                self.vote_accounts.sub_stake(&old_voter_pubkey, old_stake);
                self.vote_accounts.add_stake(&voter_pubkey, stake);
            }
        } else {
            // New stake delegation - add to delegated_stakes and vote_accounts
            self.add_delegated_stake(voter_pubkey, stake);
            self.vote_accounts.add_stake(&voter_pubkey, stake);
        }
    }

    pub(crate) fn stake_delegations(&self) -> &StakeDelegationsMaps<StakeAccount> {
        &self.stake_delegations
    }

    pub(crate) fn highest_staked_node(&self) -> Option<SlotLeader> {
        let (vote_address, vote_account) = self.vote_accounts.find_max_by_delegated_stake()?;
        Some(SlotLeader {
            id: *vote_account.node_pubkey(),
            vote_address: *vote_address,
        })
    }
}

/// Macro to generate `From<Stakes<From>> for Stakes<To>` impls.
#[cfg(feature = "dev-context-only-utils")]
macro_rules! impl_stake_format_conversion {
    ($from:ty, $to:ty, |$binding:ident| $expr:expr) => {
        /// This conversion is memory intensive so should only be used in development contexts.
        impl From<Stakes<$from>> for Stakes<$to> {
            fn from(stakes: Stakes<$from>) -> Self {
                let Stakes {
                    vote_accounts,
                    stake_delegations,
                    delegated_stakes: _,
                    unused,
                    epoch,
                    stake_history,
                } = stakes;
                let StakeDelegationsMaps {
                    snapshot,
                    overrides,
                    additions,
                    removals,
                } = stake_delegations;
                let snapshot = snapshot
                    .iter()
                    .map(|(pubkey, $binding)| (*pubkey, $expr))
                    .collect();
                let snapshot = Arc::new(snapshot);
                let overrides = overrides
                    .into_iter()
                    .map(|(pubkey, $binding)| (pubkey, $expr))
                    .collect();
                let additions = additions
                    .into_iter()
                    .map(|(pubkey, $binding)| (pubkey, $expr))
                    .collect();
                Self {
                    vote_accounts,
                    stake_delegations: StakeDelegationsMaps {
                        snapshot,
                        overrides,
                        additions,
                        removals,
                    },
                    delegated_stakes: DelegatedStakes::default(),
                    unused,
                    epoch,
                    stake_history,
                }
            }
        }
    };
}

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(StakeAccount, Delegation, |sa| *sa.delegation());

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(StakeAccount, Stake, |sa| *sa.stake());

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(Stake, Delegation, |stake| stake.delegation);

fn merge_delegated_stakes(
    mut stakes: HashMap</*voter:*/ Pubkey, /*stake:*/ u64>,
    other: HashMap</*voter:*/ Pubkey, /*stake:*/ u64>,
) -> HashMap</*voter:*/ Pubkey, /*stake:*/ u64> {
    if stakes.len() < other.len() {
        return merge_delegated_stakes(other, stakes);
    }
    for (pubkey, stake) in other {
        *stakes.entry(pubkey).or_default() += stake;
    }
    stakes
}

fn refresh_vote_accounts(
    thread_pool: &ThreadPool,
    epoch: Epoch,
    vote_accounts: &VoteAccounts,
    stake_delegations: &IndexedStakeDelegations<'_, StakeAccount>,
    stake_history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
    use_fixed_point_stake_math: bool,
) -> (VoteAccounts, DelegatedStakes) {
    fn merge(mut stakes: DelegatedStakes, other: DelegatedStakes) -> DelegatedStakes {
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
            .par_iter_unfiltered()
            .flatten()
            .fold(
                DelegatedStakes::default,
                |mut delegated_stakes, (_stake_pubkey, stake_account)| {
                    let delegation = stake_account.delegation();
                    let stake = delegation_effective_stake(
                        delegation,
                        epoch,
                        stake_history,
                        new_rate_activation_epoch,
                        use_fixed_point_stake_math,
                    );
                    if stake != 0 {
                        *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
                    }
                    delegated_stakes
                },
            )
            .reduce(DelegatedStakes::default, merge)
    });
    let vote_accounts = vote_accounts
        .iter()
        .map(|(&vote_pubkey, vote_account)| {
            let delegated_stake = delegated_stakes
                .get(&vote_pubkey)
                .copied()
                .unwrap_or_default();
            (vote_pubkey, (delegated_stake, vote_account.clone()))
        })
        .collect();
    (vote_accounts, delegated_stakes)
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::{stake_delegation::effective_stake, stake_utils},
        rayon::ThreadPoolBuilder,
        solana_account::WritableAccount,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_stake_interface::{self as stake, state::StakeStateV2},
        solana_vote_interface::state::{BLS_PUBLIC_KEY_COMPRESSED_SIZE, VoteStateV4},
        solana_vote_program::vote_state,
    };

    impl Stakes<Delegation> {
        /// Convert deserialized stakes into runtime stakes representation
        pub(crate) fn from_deserialized(stakes: DeserializableDelegationStakes) -> Self {
            let DeserializableDelegationStakes {
                vote_accounts,
                stake_delegations,
                unused,
                epoch,
                stake_history,
            } = stakes;
            let stake_delegations = StakeDelegationsMaps {
                snapshot: Arc::new(IndexMap::from_iter(stake_delegations)),
                overrides: ImblGenericHashMap::default(),
                additions: ImblHashMap::default(),
                removals: ImblHashSet::default(),
            };
            Self {
                vote_accounts,
                stake_delegations,
                delegated_stakes: DelegatedStakes::default(),
                unused: unused,
                epoch: epoch,
                stake_history: stake_history,
            }
        }
    }

    #[test]
    fn test_stake_delegations_view_overlay() {
        let root_only = Pubkey::new_unique();
        let overridden = Pubkey::new_unique();
        let removed = Pubkey::new_unique();
        let unrooted_only = Pubkey::new_unique();

        let snapshot =
            StakeDelegationsSnapshot::from_iter([(root_only, 1), (overridden, 2), (removed, 3)]);
        let overridden_ix = snapshot.get_index_of(&overridden).unwrap();
        let snapshot = Arc::new(snapshot);
        let overrides = ImblGenericHashMap::from_iter([(overridden_ix, 20)]);
        let additions = ImblHashMap::from_iter([(unrooted_only, 30)]);
        let removals = [&removed].into_iter().collect();
        let stake_delegations = StakeDelegationsMaps {
            snapshot,
            overrides,
            additions,
            removals,
        };
        let expected = HashMap::from([(root_only, 1), (overridden, 20), (unrooted_only, 30)]);

        assert_eq!(stake_delegations.len(), expected.len());
        assert_eq!(stake_delegations.get(&removed), None);
        let serial_delegations = stake_delegations.iter().collect::<Vec<_>>();
        assert_eq!(serial_delegations.len(), expected.len());
        assert_eq!(
            serial_delegations
                .into_iter()
                .map(|(pubkey, stake_delegation)| (*pubkey, *stake_delegation))
                .collect::<HashMap<_, _>>(),
            expected,
        );

        let indexed = stake_delegations.indexed();
        let parallel_delegation_slots = indexed.par_iter_unfiltered().collect::<Vec<_>>();
        assert_eq!(
            parallel_delegation_slots.len(),
            expected.len().saturating_add(1)
        );
        assert_eq!(
            parallel_delegation_slots
                .iter()
                .filter(|stake_delegation| stake_delegation.is_none())
                .count(),
            1,
        );
        let parallel_delegations = parallel_delegation_slots
            .into_iter()
            .flatten()
            .map(|(pubkey, stake_delegation)| (*pubkey, *stake_delegation))
            .collect::<Vec<_>>();
        assert_eq!(parallel_delegations.len(), expected.len());
        assert_eq!(
            parallel_delegations.into_iter().collect::<HashMap<_, _>>(),
            expected,
        );
    }

    #[test]
    fn test_root_stake_delegations() {
        let root_only = Pubkey::new_unique();
        let overridden = Pubkey::new_unique();
        let removed = Pubkey::new_unique();
        let unrooted_only = Pubkey::new_unique();

        let mut snapshot = StakeDelegationsSnapshot::default();
        snapshot.insert(root_only, 1);
        snapshot.insert(overridden, 2);
        let overridden_ix = snapshot.get_index_of(&overridden).unwrap();
        snapshot.insert(removed, 3);
        let snapshot = Arc::new(snapshot);
        let overrides = ImblGenericHashMap::from_iter([(overridden_ix, 20)]);
        let additions = ImblHashMap::from_iter([(unrooted_only, 30)]);
        let removals = ImblHashSet::from_iter([removed]);
        let stake_delegations = StakeDelegationsMaps {
            snapshot,
            overrides,
            additions,
            removals,
        };
        let mut stakes = Stakes::<u64> {
            stake_delegations,
            ..Stakes::default()
        };
        let sibling_fork = stakes.clone();

        stakes.update_stake_delegations_snapshot();

        let Stakes {
            stake_delegations, ..
        } = stakes;
        let StakeDelegationsMaps {
            snapshot: updated_snapshot,
            overrides: updated_overrides,
            additions: updated_additions,
            removals: updated_removals,
        } = stake_delegations;
        assert_eq!(updated_snapshot.len(), 3);
        assert_eq!(updated_snapshot.get(&root_only), Some(&1));
        assert_eq!(updated_snapshot.get(&overridden), Some(&20));
        assert_eq!(updated_snapshot.get(&removed), None);
        assert_eq!(updated_snapshot.get(&unrooted_only), Some(&30));
        assert!(updated_overrides.is_empty());
        assert!(updated_additions.is_empty());
        assert!(updated_removals.is_empty());

        let Stakes {
            stake_delegations: sibling_fork_stake_delegations,
            ..
        } = sibling_fork;
        let StakeDelegationsMaps {
            snapshot: sibling_fork_snapshot,
            overrides: sibling_fork_overrides,
            additions: sibling_fork_additions,
            removals: sibling_fork_removals,
        } = sibling_fork_stake_delegations;
        assert_eq!(sibling_fork_snapshot.len(), 3);
        assert_eq!(sibling_fork_snapshot.get(&overridden), Some(&2));
        assert_eq!(sibling_fork_overrides.get(&overridden_ix), Some(&20));
        assert_eq!(
            sibling_fork_additions.iter().collect::<Vec<_>>(),
            [(&unrooted_only, &30)]
        );
        assert!(sibling_fork_removals.contains(&removed));
    }

    //  set up some dummies for a staked node     ((     vote      )  (     stake     ))
    pub(crate) fn create_staked_node_accounts(
        stake: u64,
        rent: &Rent,
    ) -> ((Pubkey, AccountSharedData), (Pubkey, AccountSharedData)) {
        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();
        let vote_account = vote_state::create_v4_account_with_authorized(
            &node_pubkey,
            &vote_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &vote_pubkey,
            0,
            &vote_pubkey,
            0,
            &node_pubkey,
            1,
        );
        let stake_pubkey = solana_pubkey::new_rand();
        (
            (vote_pubkey, vote_account),
            (
                stake_pubkey,
                create_stake_account(stake, &vote_pubkey, &stake_pubkey, rent),
            ),
        )
    }

    //   add stake to a vote_pubkey                               (   stake    )
    pub(crate) fn create_stake_account(
        stake: u64,
        vote_pubkey: &Pubkey,
        stake_pubkey: &Pubkey,
        rent: &Rent,
    ) -> AccountSharedData {
        let node_pubkey = solana_pubkey::new_rand();
        let lamports = rent.minimum_balance(StakeStateV2::size_of()) + stake;
        stake_utils::create_stake_account(
            stake_pubkey,
            vote_pubkey,
            &vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                vote_pubkey,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                vote_pubkey,
                0,
                vote_pubkey,
                0,
                &node_pubkey,
                1,
            ),
            rent,
            lamports,
        )
    }

    #[test]
    fn test_stakes_basic() {
        for i in 0..4 {
            let stakes_cache = StakesCache::new(Stakes {
                epoch: i,
                ..Stakes::default()
            });
            let rent = Rent::default();

            let ((vote_pubkey, vote_account), (stake_pubkey, mut stake_account)) =
                create_staked_node_accounts(10, &rent);

            stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
            let stake = stake_account
                .deserialize_data::<StakeStateV2>()
                .unwrap()
                .stake()
                .unwrap();
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                let expected_stake =
                    effective_stake(&stake, i, &StakeHistory::default(), None, true);
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    expected_stake
                );
            }

            stake_account.set_lamports(42);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                let expected_stake =
                    effective_stake(&stake, i, &StakeHistory::default(), None, true);
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    expected_stake
                ); // stays old stake, because only 10 is activated
            }

            // activate more
            let mut stake_account =
                create_stake_account(42, &vote_pubkey, &solana_pubkey::new_rand(), &rent);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
            let stake = stake_account
                .deserialize_data::<StakeStateV2>()
                .unwrap()
                .stake()
                .unwrap();
            {
                let stakes = stakes_cache.stakes();
                let vote_accounts = stakes.vote_accounts();
                assert!(vote_accounts.get(&vote_pubkey).is_some());
                let expected_stake =
                    effective_stake(&stake, i, &StakeHistory::default(), None, true);
                assert_eq!(
                    vote_accounts.get_delegated_stake(&vote_pubkey),
                    expected_stake
                ); // now stake of 42 is activated
            }

            stake_account.set_lamports(0);
            stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
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
        let rent = Rent::default();

        assert_eq!(stakes_cache.stakes().highest_staked_node(), None);

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);

        let ((vote11_pubkey, vote11_account), (stake11_pubkey, stake11_account)) =
            create_staked_node_accounts(20, &rent);

        stakes_cache.check_and_store(&vote11_pubkey, &vote11_account, None, true);
        stakes_cache.check_and_store(&stake11_pubkey, &stake11_account, None, true);

        let vote11_node_pubkey = VoteStateV4::deserialize(vote11_account.data(), &vote11_pubkey)
            .unwrap()
            .node_pubkey;

        let highest_staked_node = stakes_cache.stakes().highest_staked_node();
        assert_eq!(
            highest_staked_node,
            Some(SlotLeader {
                id: vote11_node_pubkey,
                vote_address: vote11_pubkey,
            })
        );
    }

    #[test]
    fn test_stakes_vote_account_disappear_reappear() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });
        let rent = Rent::default();

        let ((vote_pubkey, mut vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 10);
        }

        vote_account.set_lamports(0);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        vote_account.set_lamports(1);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

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
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        // Vote account uninitialized
        vote_account.set_data(vec![0; VoteStateV4::size_of()]);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_none());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }

        vote_account.set_data(cache_data);
        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

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
        let rent = Rent::default();

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        let ((vote_pubkey2, vote_account2), (_stake_pubkey2, stake_account2)) =
            create_staked_node_accounts(10, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
        stakes_cache.check_and_store(&vote_pubkey2, &vote_account2, None, true);

        // delegates to vote_pubkey
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);

        let stake = stake_account
            .deserialize_data::<StakeStateV2>()
            .unwrap()
            .stake()
            .unwrap();

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            let expected_stake =
                effective_stake(&stake, stakes.epoch, &stakes.stake_history, None, true);
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                expected_stake
            );
            assert!(vote_accounts.get(&vote_pubkey2).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey2), 0);
        }

        // delegates to vote_pubkey2
        stakes_cache.check_and_store(&stake_pubkey, &stake_account2, None, true);

        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
            assert!(vote_accounts.get(&vote_pubkey2).is_some());
            let expected_stake =
                effective_stake(&stake, stakes.epoch, &stakes.stake_history, None, true);
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey2),
                expected_stake
            );
        }
    }
    #[test]
    fn test_stakes_multiple_stakers() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });
        let rent = Rent::default();

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        let stake_pubkey2 = solana_pubkey::new_rand();
        let stake_account2 = create_stake_account(10, &vote_pubkey, &stake_pubkey2, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);

        // delegates to vote_pubkey
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
        stakes_cache.check_and_store(&stake_pubkey2, &stake_account2, None, true);

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
        let rent = Rent::default();

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);
        let stake = stake_account
            .deserialize_data::<StakeStateV2>()
            .unwrap()
            .stake()
            .unwrap();

        let initial_expected_stake = {
            let stakes = stakes_cache.stakes();
            effective_stake(&stake, stakes.epoch, &stakes.stake_history, None, true)
        };
        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                initial_expected_stake
            );
        }
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let next_epoch = 3;
        let (stake_history, vote_accounts, delegated_stakes, effective_delegated_stakes) = {
            let stakes = stakes_cache.stakes();
            let stake_delegations = stakes.stake_delegations().indexed();
            stakes.calculate_activated_stake(
                next_epoch,
                &thread_pool,
                None,
                &stake_delegations,
                true,
            )
        };
        assert_eq!(
            effective_delegated_stakes
                .delegated_stakes
                .get(&vote_pubkey)
                .copied(),
            Some(initial_expected_stake)
        );
        stakes_cache.activate_epoch(next_epoch, stake_history, vote_accounts, delegated_stakes);
        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            let expected_stake =
                effective_stake(&stake, stakes.epoch, &stakes.stake_history, None, true);
            assert_eq!(
                vote_accounts.get_delegated_stake(&vote_pubkey),
                expected_stake
            );
        }
    }

    #[test]
    fn test_stakes_not_delegate() {
        let stakes_cache = StakesCache::new(Stakes {
            epoch: 4,
            ..Stakes::default()
        });
        let rent = Rent::default();

        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &rent);

        stakes_cache.check_and_store(&vote_pubkey, &vote_account, None, true);
        stakes_cache.check_and_store(&stake_pubkey, &stake_account, None, true);

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
            true,
        );
        {
            let stakes = stakes_cache.stakes();
            let vote_accounts = stakes.vote_accounts();
            assert!(vote_accounts.get(&vote_pubkey).is_some());
            assert_eq!(vote_accounts.get_delegated_stake(&vote_pubkey), 0);
        }
    }
}
