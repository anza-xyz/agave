#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMut},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_stake_interface::{
        program as stake_program,
        state::{Delegation, Stake, StakeStateV2},
    },
    std::marker::PhantomData,
    thiserror::Error,
    wincode::SchemaWrite,
};
#[cfg(feature = "frozen-abi")]
use {
    solana_frozen_abi::{abi_example::AbiExample, stable_abi::StableAbi},
    solana_stake_interface::{stake_flags::StakeFlags, state::Meta},
};

/// An account and a stake state deserialized from the account.
/// Generic type T enforces type-safety so that StakeAccount<Delegation> can
/// only wrap a stake-state which is a Delegation; whereas StakeAccount<()>
/// wraps any account with stake state.
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Default)]
pub struct StakeAccount<T> {
    // Skipped by the custom (delegation/stake-format) serializer; sample the default.
    #[cfg_attr(feature = "frozen-abi", stable_abi_sample(with = "Default::default()"))]
    account: AccountSharedData,
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_delegated_stake_state(rng)")
    )]
    stake_state: StakeStateV2,
    _phantom: PhantomData<T>,
}

// `StakeAccount<Delegation>` is only ever wincode-serialized as part of the snapshot's
// `stake_delegations` map, which uses the delegation format: just the `Delegation` is written (see
// `serialize_stake_accounts_to_delegation_format`). Mirror that here so the wincode bytes match
// bincode; `FromIntoIterator` on the map picks up this per-value schema automatically.
unsafe impl<C: wincode::config::Config> SchemaWrite<C> for StakeAccount<Delegation> {
    type Src = Self;

    const TYPE_META: wincode::TypeMeta =
        <Delegation as SchemaWrite<C>>::TYPE_META.keep_zero_copy(false);

    fn size_of(src: &Self::Src) -> wincode::WriteResult<usize> {
        <Delegation as SchemaWrite<C>>::size_of(src.delegation())
    }

    fn write(writer: impl wincode::io::Writer, src: &Self::Src) -> wincode::WriteResult<()> {
        <Delegation as SchemaWrite<C>>::write(writer, src.delegation())
    }
}

/// Samples a random `StakeStateV2::Stake`; the delegation-format serializer unwraps
/// `delegation_ref()`, which would panic on any other variant.
#[cfg(feature = "frozen-abi")]
fn sample_delegated_stake_state(
    rng: &mut (impl solana_frozen_abi::rand::RngCore + ?Sized),
) -> StakeStateV2 {
    StakeStateV2::Stake(
        Meta::random(rng),
        Stake::random(rng),
        StakeFlags::random(rng),
    )
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    InstructionError(#[from] InstructionError),
    #[error("Invalid delegation: {0:?}")]
    InvalidDelegation(Box<StakeStateV2>),
    #[error("Invalid stake account owner: {0}")]
    InvalidOwner(/*owner:*/ Pubkey),
}

impl<T> StakeAccount<T> {
    #[inline]
    pub(crate) fn lamports(&self) -> u64 {
        self.account.lamports()
    }

    #[inline]
    pub(crate) fn stake_state(&self) -> &StakeStateV2 {
        &self.stake_state
    }

    #[inline]
    pub(crate) fn data_len(&self) -> usize {
        self.account.data().len()
    }
}

impl StakeAccount<Delegation> {
    #[inline]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn delegation(&self) -> &Delegation {
        // Safe to unwrap here because StakeAccount<Delegation> will always
        // only wrap a stake-state which is a delegation.
        self.stake_state.delegation_ref().unwrap()
    }

    #[inline]
    pub(crate) fn stake(&self) -> &Stake {
        // Safe to unwrap here because StakeAccount<Delegation> will always
        // only wrap a stake-state.
        self.stake_state.stake_ref().unwrap()
    }
}

impl TryFrom<AccountSharedData> for StakeAccount<Delegation> {
    type Error = Error;
    fn try_from(account: AccountSharedData) -> Result<Self, Self::Error> {
        if account.owner() != &stake_program::id() {
            return Err(Error::InvalidOwner(*account.owner()));
        }
        let stake_state: StakeStateV2 = account.state()?;
        if stake_state.delegation().is_none() {
            return Err(Error::InvalidDelegation(Box::new(stake_state)));
        }
        Ok(Self {
            account,
            stake_state,
            _phantom: PhantomData,
        })
    }
}

impl<T> From<StakeAccount<T>> for (AccountSharedData, StakeStateV2) {
    #[inline]
    fn from(stake_account: StakeAccount<T>) -> Self {
        (stake_account.account, stake_account.stake_state)
    }
}

impl<S, T> PartialEq<StakeAccount<S>> for StakeAccount<T> {
    fn eq(&self, other: &StakeAccount<S>) -> bool {
        let StakeAccount {
            account,
            stake_state,
            _phantom,
        } = other;
        account == &self.account && stake_state == &self.stake_state
    }
}

#[cfg(feature = "frozen-abi")]
impl AbiExample for StakeAccount<Delegation> {
    fn example() -> Self {
        use solana_account::Account;
        let stake_state =
            StakeStateV2::Stake(Meta::example(), Stake::example(), StakeFlags::example());
        let mut account = Account::example();
        account.data.resize(200, 0u8);
        account.owner = stake_program::id();
        account.set_state(&stake_state).unwrap();
        Self::try_from(AccountSharedData::from(account)).unwrap()
    }
}
