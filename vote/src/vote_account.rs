#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::field_qualifiers;
// The rng traits come straight from `rand` when sampling for tests, and from frozen-abi's re-export
// when sampling for abi digests (which enables `frozen-abi` without the `rand` dependency). Both
// resolve to the same `rand` crate, so `sample_vote_account` is shared between the two.
#[cfg(feature = "dev-context-only-utils")]
use rand::{Rng, RngCore};
#[cfg(all(feature = "frozen-abi", not(feature = "dev-context-only-utils")))]
use solana_frozen_abi::rand::{Rng, RngCore};
use {
    crate::vote_state_view::VoteStateView,
    log::*,
    serde::{Deserialize, Deserializer, Serialize, ser::Serializer},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_transaction::SchemaWrite,
    std::{
        cmp::Ordering,
        collections::{HashMap, hash_map::Entry},
        iter::FromIterator,
        mem,
        sync::{Arc, OnceLock},
    },
    thiserror::Error,
    wincode::{
        ReadError, ReadResult, SchemaRead, TypeMeta, WriteResult, config::Config, io::Reader,
    },
};
#[cfg(any(feature = "dev-context-only-utils", feature = "frozen-abi"))]
use {
    solana_bls_signatures::Keypair as BLSKeypair,
    solana_clock::Clock,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_vote_interface::state::{
        BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE, VoteInitV2, VoteStateV4, VoteStateVersions,
    },
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, PartialEq)]
pub struct VoteAccount(Arc<VoteAccountInner>);

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    InstructionError(#[from] InstructionError),
    #[error("Invalid vote account owner: {0}")]
    InvalidOwner(/*owner:*/ Pubkey),
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
struct VoteAccountInner {
    account: AccountSharedData,
    vote_state_view: VoteStateView,
}

pub type VoteAccountsHashMap = HashMap<Pubkey, (/*stake:*/ u64, VoteAccount)>;
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Debug, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(vote_accounts(pub))
)]
pub struct VoteAccounts {
    vote_accounts: Arc<VoteAccountsHashMap>,
    // Inner Arc is meant to implement copy-on-write semantics.
    #[cfg_attr(feature = "frozen-abi", stable_abi_sample(with = "Default::default()"))]
    #[serde(skip)]
    #[wincode(skip)]
    staked_nodes: OnceLock<
        Arc<
            HashMap<
                Pubkey, // VoteAccount.vote_state.node_pubkey.
                u64,    // Total stake across all vote-accounts.
            >,
        >,
    >,
}

impl Clone for VoteAccounts {
    fn clone(&self) -> Self {
        Self {
            vote_accounts: Arc::clone(&self.vote_accounts),
            // Reset this so that if the previous bank did compute `staked_nodes`, the new bank
            // won't copy-on-write and keep updating the map if the staked nodes on this bank are
            // never accessed. See [`VoteAccounts::add_stake`] [`VoteAccounts::sub_stake`] and
            // [`VoteAccounts::staked_nodes`].
            staked_nodes: OnceLock::new(),
        }
    }
}

impl VoteAccount {
    pub fn account(&self) -> &AccountSharedData {
        &self.0.account
    }

    pub fn lamports(&self) -> u64 {
        self.0.account.lamports()
    }

    pub fn owner(&self) -> &Pubkey {
        self.0.account.owner()
    }

    pub fn vote_state_view(&self) -> &VoteStateView {
        &self.0.vote_state_view
    }

    /// VoteState.node_pubkey of this vote-account.
    pub fn node_pubkey(&self) -> &Pubkey {
        self.0.vote_state_view.node_pubkey()
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_random() -> VoteAccount {
        Self::sample_vote_account(&mut rand::rng())
    }

    /// Samples a valid, parseable vote account (owner = vote program, data = a well-formed
    /// `VoteStateV4` with a real BLS keypair derived from `rng`) from `rng`. Shared by `new_random`
    /// and the frozen-abi `StableAbi` sampler.
    #[cfg(any(feature = "dev-context-only-utils", feature = "frozen-abi"))]
    fn sample_vote_account(rng: &mut (impl RngCore + ?Sized)) -> VoteAccount {
        const BLS_KEYPAIR_DERIVE_SEED: &[u8; 9] = b"alpenglow";

        let keypair = Keypair::new_from_array(rng.random());
        let bls_keypair =
            BLSKeypair::derive_from_signer(&keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap();
        let vote_init = VoteInitV2 {
            node_pubkey: Pubkey::from(rng.random::<[u8; 32]>()),
            authorized_voter: keypair.pubkey(),
            authorized_voter_bls_pubkey: bls_keypair.public.to_bytes_compressed(),
            // A valid proof of possession isn't required for the account to parse.
            authorized_voter_bls_proof_of_possession: [0; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
            authorized_withdrawer: Pubkey::from(rng.random::<[u8; 32]>()),
            inflation_rewards_commission_bps: rng.random_range(0..10_000),
            block_revenue_commission_bps: rng.random_range(0..10_000),
        };
        let clock = Clock {
            slot: rng.random(),
            epoch_start_timestamp: rng.random(),
            epoch: rng.random(),
            leader_schedule_epoch: rng.random(),
            unix_timestamp: rng.random(),
        };
        let vote_state = VoteStateV4::new(
            &vote_init,
            &Pubkey::from(rng.random::<[u8; 32]>()),
            &Pubkey::from(rng.random::<[u8; 32]>()),
            &clock,
        );
        let data = wincode::serialize(&VoteStateVersions::new_v4(vote_state)).unwrap();
        let mut account = AccountSharedData::new(
            rng.random(), // lamports
            data.len(),
            &solana_sdk_ids::vote::id(), // owner
        );
        account.set_data_from_slice(&data);
        VoteAccount::try_from(account).unwrap()
    }
}

impl VoteAccounts {
    pub fn len(&self) -> usize {
        self.vote_accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vote_accounts.is_empty()
    }

    pub fn staked_nodes(&self) -> Arc<HashMap</*node_pubkey:*/ Pubkey, /*stake:*/ u64>> {
        self.staked_nodes
            .get_or_init(|| {
                // Count non-zero stake accounts for optimal capacity allocation
                let non_zero_count = self
                    .vote_accounts
                    .values()
                    .filter(|(stake, _)| *stake != 0)
                    .count();

                let mut staked_nodes = HashMap::with_capacity(non_zero_count);

                for (stake, vote_account) in self.vote_accounts.values() {
                    if *stake != 0 {
                        *staked_nodes.entry(*vote_account.node_pubkey()).or_default() += *stake;
                    }
                }

                Arc::new(staked_nodes)
            })
            .clone()
    }

    // This implements the filtering logic described in SIMD-357.
    // 1. Filter out any vote accounts without BLS pubkey
    // 2. Given minimum_vote_account_balance, filter out any vote account
    //    without required balance
    // 3. If we have more than max_vote_accounts vote accounts after above
    //    filtering, sort by stake and truncate
    // 4. If any vote account in the resulting list has the same stake as any
    //    truncated vote account, just remove this vote account as well (A and
    //    B have the same stake amount, it's unfair to keep A in and kick B out
    //    just because of Pubkey difference, because that can be grinded)
    // 5. If we end up with an empty list (can happen if everyone in the world
    //    has the same stake, happens in tests, doesn't happen in real world),
    //    log a warning
    pub fn clone_and_filter_for_vat(
        &self,
        max_vote_accounts: usize,
        minimum_vote_account_balance: u64,
    ) -> VoteAccounts {
        assert!(max_vote_accounts > 0, "max_vote_accounts must be > 0");
        let capacity = max_vote_accounts.min(self.vote_accounts.len());
        let mut entries_to_sort: Vec<(&Pubkey, &VoteAccount, u64)> = Vec::with_capacity(capacity);
        for (pubkey, (stake, vote_account)) in self.vote_accounts.iter() {
            let has_bls = vote_account
                .vote_state_view()
                .bls_pubkey_compressed()
                .is_some();
            let has_stake = *stake != 0u64;
            let has_balance = vote_account.lamports() >= minimum_vote_account_balance;

            if !has_bls || !has_stake || !has_balance {
                continue;
            }
            entries_to_sort.push((pubkey, vote_account, *stake));
        }

        let valid_len = entries_to_sort.len();
        if entries_to_sort.len() > max_vote_accounts {
            // Find the cutoff stake using partial sort (more efficient than full sort).
            let (_, cutoff_entry, _) =
                entries_to_sort.select_nth_unstable_by(max_vote_accounts, |a, b| b.2.cmp(&a.2));
            let floor_stake = cutoff_entry.2;

            // Per SIMD 357, we remove all vote accounts with stake smaller or equal to
            // the first truncated one.
            entries_to_sort.retain(|(_, _, stake)| *stake > floor_stake);
        }

        let mut top_entries: HashMap<Pubkey, (u64, VoteAccount)> =
            HashMap::with_capacity(entries_to_sort.len());
        top_entries.extend(
            entries_to_sort
                .into_iter()
                .map(|(pubkey, vote_account, stake)| (*pubkey, (stake, vote_account.clone()))),
        );
        if top_entries.is_empty() {
            if cfg!(test) {
                info!("No valid vote accounts found");
            } else {
                error!("No valid vote accounts found");
            }
        }
        info!(
            "Out of {} vote accounts, {} are valid vote accounts after filtering, {} remain after \
             truncation",
            self.vote_accounts.len(),
            valid_len,
            top_entries.len()
        );
        VoteAccounts {
            vote_accounts: Arc::new(top_entries),
            staked_nodes: OnceLock::new(),
        }
    }

    pub fn get(&self, pubkey: &Pubkey) -> Option<&VoteAccount> {
        let (_stake, vote_account) = self.vote_accounts.get(pubkey)?;
        Some(vote_account)
    }

    pub fn get_delegated_stake(&self, pubkey: &Pubkey) -> u64 {
        self.vote_accounts
            .get(pubkey)
            .map(|(stake, _vote_account)| *stake)
            .unwrap_or_default()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Pubkey, &VoteAccount)> {
        self.vote_accounts
            .iter()
            .map(|(vote_pubkey, (_stake, vote_account))| (vote_pubkey, vote_account))
    }

    pub fn delegated_stakes(&self) -> impl Iterator<Item = (&Pubkey, u64)> {
        self.vote_accounts
            .iter()
            .map(|(vote_pubkey, (stake, _vote_account))| (vote_pubkey, *stake))
    }

    pub fn find_max_by_delegated_stake(&self) -> Option<(&Pubkey, &VoteAccount)> {
        let key = |(pubkey, (stake, _vote_account)): &(_, &(u64, _))| (*stake, *pubkey);
        let (vote_address, (_stake, vote_account)) = self.vote_accounts.iter().max_by_key(key)?;
        Some((vote_address, vote_account))
    }

    pub fn insert(
        &mut self,
        pubkey: Pubkey,
        new_vote_account: VoteAccount,
        calculate_stake: impl FnOnce() -> u64,
    ) -> Option<VoteAccount> {
        let vote_accounts = Arc::make_mut(&mut self.vote_accounts);
        match vote_accounts.entry(pubkey) {
            Entry::Occupied(mut entry) => {
                // This is an upsert, we need to update the vote state and move the stake if needed.
                let (stake, old_vote_account) = entry.get_mut();

                if let Some(staked_nodes) = self.staked_nodes.get_mut() {
                    let old_node_pubkey = old_vote_account.node_pubkey();
                    let new_node_pubkey = new_vote_account.node_pubkey();
                    if new_node_pubkey != old_node_pubkey {
                        // The node keys have changed, we move the stake from the old node to the
                        // new one
                        Self::do_sub_node_stake(staked_nodes, *stake, old_node_pubkey);
                        Self::do_add_node_stake(staked_nodes, *stake, *new_node_pubkey);
                    }
                }

                // Update the vote state
                Some(mem::replace(old_vote_account, new_vote_account))
            }
            Entry::Vacant(entry) => {
                // This is a new vote account. Use the caller-provided stake for the initial
                // cache entry.
                let (stake, vote_account) = entry.insert((calculate_stake(), new_vote_account));
                if let Some(staked_nodes) = self.staked_nodes.get_mut() {
                    Self::do_add_node_stake(staked_nodes, *stake, *vote_account.node_pubkey());
                }
                None
            }
        }
    }

    pub fn remove(&mut self, pubkey: &Pubkey) -> Option<(u64, VoteAccount)> {
        let vote_accounts = Arc::make_mut(&mut self.vote_accounts);
        let entry = vote_accounts.remove(pubkey);
        if let Some((stake, ref vote_account)) = entry {
            self.sub_node_stake(stake, vote_account);
        }
        entry
    }

    pub fn add_stake(&mut self, pubkey: &Pubkey, delta: u64) {
        let vote_accounts = Arc::make_mut(&mut self.vote_accounts);
        if let Some((stake, vote_account)) = vote_accounts.get_mut(pubkey) {
            *stake += delta;
            let vote_account = vote_account.clone();
            self.add_node_stake(delta, &vote_account);
        }
    }

    pub fn sub_stake(&mut self, pubkey: &Pubkey, delta: u64) {
        let vote_accounts = Arc::make_mut(&mut self.vote_accounts);
        if let Some((stake, vote_account)) = vote_accounts.get_mut(pubkey) {
            *stake = stake
                .checked_sub(delta)
                .expect("subtraction value exceeds account's stake");
            let vote_account = vote_account.clone();
            self.sub_node_stake(delta, &vote_account);
        }
    }

    fn add_node_stake(&mut self, stake: u64, vote_account: &VoteAccount) {
        let Some(staked_nodes) = self.staked_nodes.get_mut() else {
            return;
        };

        VoteAccounts::do_add_node_stake(staked_nodes, stake, *vote_account.node_pubkey());
    }

    fn do_add_node_stake(
        staked_nodes: &mut Arc<HashMap<Pubkey, u64>>,
        stake: u64,
        node_pubkey: Pubkey,
    ) {
        if stake == 0u64 {
            return;
        }

        Arc::make_mut(staked_nodes)
            .entry(node_pubkey)
            .and_modify(|s| *s += stake)
            .or_insert(stake);
    }

    fn sub_node_stake(&mut self, stake: u64, vote_account: &VoteAccount) {
        let Some(staked_nodes) = self.staked_nodes.get_mut() else {
            return;
        };

        VoteAccounts::do_sub_node_stake(staked_nodes, stake, vote_account.node_pubkey());
    }

    fn do_sub_node_stake(
        staked_nodes: &mut Arc<HashMap<Pubkey, u64>>,
        stake: u64,
        node_pubkey: &Pubkey,
    ) {
        if stake == 0u64 {
            return;
        }

        let staked_nodes = Arc::make_mut(staked_nodes);
        let current_stake = staked_nodes
            .get_mut(node_pubkey)
            .expect("this should not happen");
        match (*current_stake).cmp(&stake) {
            Ordering::Less => panic!("subtraction value exceeds node's stake"),
            Ordering::Equal => {
                staked_nodes.remove(node_pubkey);
            }
            Ordering::Greater => *current_stake -= stake,
        }
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::stable_abi::StableAbi for VoteAccount {
    fn random_with_context(rng: &mut (impl RngCore + ?Sized), _ctx: ()) -> Self {
        Self::sample_vote_account(rng)
    }
}

// `VoteAccount` serializes only its `account` (see the `Serialize` impl below). Mirror that for
// wincode so the snapshot wire format matches bincode: `vote_state_view` is a parsed view of the
// account data, rebuilt on read, and is intentionally not written.
unsafe impl<C: wincode::config::Config> SchemaWrite<C> for VoteAccount {
    type Src = Self;

    const TYPE_META: TypeMeta =
        <AccountSharedData as SchemaWrite<C>>::TYPE_META.keep_zero_copy(false);

    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        <AccountSharedData as SchemaWrite<C>>::size_of(&src.0.account)
    }

    fn write(writer: impl wincode::io::Writer, src: &Self::Src) -> WriteResult<()> {
        <AccountSharedData as SchemaWrite<C>>::write(writer, &src.0.account)
    }
}

impl Serialize for VoteAccount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.account.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VoteAccount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let account = AccountSharedData::deserialize(deserializer)?;
        VoteAccount::try_from(account).map_err(serde::de::Error::custom)
    }
}

// Read-counterpart of the custom `SchemaWrite` above: read the inner `AccountSharedData` (the only
// thing written) and rebuild the parsed `vote_state_view` via `try_from`, mirroring the `Deserialize`
// impl. `VoteAccounts` then derives `SchemaRead` on top of this, just like the serde path.
unsafe impl<'de, C: Config> SchemaRead<'de, C> for VoteAccount {
    type Dst = Self;

    fn read(reader: impl Reader<'de>, dst: &mut mem::MaybeUninit<Self::Dst>) -> ReadResult<()> {
        let account = <AccountSharedData as SchemaRead<'de, C>>::get(reader)?;
        let vote_account = VoteAccount::try_from(account)
            .map_err(|_| ReadError::InvalidValue("invalid vote account"))?;
        dst.write(vote_account);
        Ok(())
    }
}

impl<'a> From<&'a VoteAccount> for AccountSharedData {
    fn from(account: &'a VoteAccount) -> Self {
        account.0.account.clone()
    }
}

impl From<VoteAccount> for AccountSharedData {
    fn from(account: VoteAccount) -> Self {
        account.0.account.clone()
    }
}

impl TryFrom<AccountSharedData> for VoteAccount {
    type Error = Error;
    fn try_from(account: AccountSharedData) -> Result<Self, Self::Error> {
        if !solana_sdk_ids::vote::check_id(account.owner()) {
            return Err(Error::InvalidOwner(*account.owner()));
        }

        Ok(Self(Arc::new(VoteAccountInner {
            vote_state_view: VoteStateView::try_new(account.data_clone())
                .map_err(|_| Error::InstructionError(InstructionError::InvalidAccountData))?,
            account,
        })))
    }
}

impl PartialEq<VoteAccountInner> for VoteAccountInner {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            account,
            vote_state_view: _,
        } = self;
        account == &other.account
    }
}

impl Default for VoteAccounts {
    fn default() -> Self {
        Self {
            vote_accounts: Arc::default(),
            staked_nodes: OnceLock::new(),
        }
    }
}

impl PartialEq<VoteAccounts> for VoteAccounts {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            vote_accounts,
            staked_nodes: _,
        } = self;
        vote_accounts == &other.vote_accounts
    }
}

impl From<Arc<VoteAccountsHashMap>> for VoteAccounts {
    fn from(vote_accounts: Arc<VoteAccountsHashMap>) -> Self {
        Self {
            vote_accounts,
            staked_nodes: OnceLock::new(),
        }
    }
}

impl AsRef<VoteAccountsHashMap> for VoteAccounts {
    fn as_ref(&self) -> &VoteAccountsHashMap {
        &self.vote_accounts
    }
}

impl From<&VoteAccounts> for Arc<VoteAccountsHashMap> {
    fn from(vote_accounts: &VoteAccounts) -> Self {
        Arc::clone(&vote_accounts.vote_accounts)
    }
}

impl FromIterator<(Pubkey, (/*stake:*/ u64, VoteAccount))> for VoteAccounts {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (Pubkey, (u64, VoteAccount))>,
    {
        Self::from(Arc::new(HashMap::from_iter(iter)))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_account::WritableAccount};

    #[test]
    #[should_panic(expected = "InvalidAccountData")]
    fn test_vote_account_try_from_invalid_account() {
        let mut account = AccountSharedData::default();
        account.set_owner(solana_sdk_ids::vote::id());
        VoteAccount::try_from(account).unwrap();
    }
}
