use {
    crate::vote_state_view::VoteStateView,
    serde::{
        Deserialize, Serialize,
        de::{MapAccess, Visitor},
        ser::Serializer,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    std::{
        cmp::Ordering,
        collections::{HashMap, hash_map::Entry},
        fmt,
        iter::FromIterator,
        mem,
        sync::{Arc, OnceLock},
    },
    thiserror::Error,
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
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VoteAccounts {
    #[serde(deserialize_with = "deserialize_accounts_hash_map")]
    vote_accounts: Arc<VoteAccountsHashMap>,
    // Inner Arc is meant to implement copy-on-write semantics.
    #[serde(skip)]
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
        use {
            rand::Rng as _,
            solana_clock::Clock,
            solana_vote_interface::state::{VoteInit, VoteStateV4, VoteStateVersions},
        };

        let mut rng = rand::rng();
        let vote_pubkey = Pubkey::new_unique();

        let vote_init = VoteInit {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: rng.random(),
        };
        let clock = Clock {
            slot: rng.random(),
            epoch_start_timestamp: rng.random(),
            epoch: rng.random(),
            leader_schedule_epoch: rng.random(),
            unix_timestamp: rng.random(),
        };
        let vote_state = VoteStateV4::new_with_defaults(&vote_pubkey, &vote_init, &clock);
        let account = AccountSharedData::new_data(
            rng.random(), // lamports
            &VoteStateVersions::new_v4(vote_state),
            &solana_sdk_ids::vote::id(), // owner
        )
        .unwrap();

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

    pub fn find_max_by_delegated_stake(&self) -> Option<&VoteAccount> {
        let key = |(_pubkey, (stake, _vote_account)): &(_, &(u64, _))| *stake;
        let (_pubkey, (_stake, vote_account)) = self.vote_accounts.iter().max_by_key(key)?;
        Some(vote_account)
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
                // This is a new vote account. We don't know the stake yet, so we need to compute it.
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

impl Serialize for VoteAccount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.account.serialize(serializer)
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

// This custom deserializer is needed to ensure compatibility at snapshot loading with versions
// before https://github.com/anza-xyz/agave/pull/2659 which would theoretically allow invalid vote
// accounts in VoteAccounts.
//
// In the (near) future we should remove this custom deserializer and make it a hard error when we
// find invalid vote accounts in snapshots.
fn deserialize_accounts_hash_map<'de, D>(
    deserializer: D,
) -> Result<Arc<VoteAccountsHashMap>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct VoteAccountsVisitor;

    impl<'de> Visitor<'de> for VoteAccountsVisitor {
        type Value = Arc<VoteAccountsHashMap>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map of vote accounts")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut accounts = HashMap::new();

            while let Some((pubkey, (stake, account))) =
                access.next_entry::<Pubkey, (u64, AccountSharedData)>()?
            {
                match VoteAccount::try_from(account) {
                    Ok(vote_account) => {
                        accounts.insert(pubkey, (stake, vote_account));
                    }
                    Err(e) => {
                        log::warn!("failed to deserialize vote account: {e}");
                    }
                }
            }

            Ok(Arc::new(accounts))
        }
    }

    deserializer.deserialize_map(VoteAccountsVisitor)
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
