//! Program state
use {
    // crate::alpenglow::accounting::{AuthorizedVoter, EpochCredit},
    crate::accounting::{AuthorizedVoter, EpochCredit},
    bytemuck::{Pod, Zeroable},
    solana_account::{AccountSharedData, WritableAccount},
    solana_bls_signatures::Pubkey as BlsPubkey,
    solana_program::{
        clock::{Epoch, Slot, UnixTimestamp},
        hash::Hash,
        program_error::ProgramError,
        pubkey::Pubkey,
        rent::Rent,
    },
    solana_vote_interface::state::BlockTimestamp as LegacyBlockTimestamp,
    spl_pod::primitives::{PodI64, PodU64},
};

pub(crate) type PodEpoch = PodU64;
pub(crate) type PodSlot = PodU64;
pub(crate) type PodUnixTimestamp = PodI64;

/// The accounting and vote information associated with
/// this vote account
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default, PartialEq)]
pub struct VoteState {
    /// The current vote state version
    pub(crate) version: u8,

    /// The node that votes in this account
    pub(crate) node_pubkey: Pubkey,

    /// The signer for withdrawals
    pub(crate) authorized_withdrawer: Pubkey,

    /// Percentage (0-100) that represents what part of a rewards
    /// payout should be given to this VoteAccount
    pub(crate) commission: u8,

    /// The signer for vote transactions in this epoch
    pub(crate) authorized_voter: AuthorizedVoter,

    /// The signer for vote transaction in an upcoming epoch
    pub(crate) next_authorized_voter: Option<AuthorizedVoter>,

    /// How many credits this validator is earning in this Epoch
    pub(crate) epoch_credits: EpochCredit,

    /// The slot of the latest replayed block
    /// Only relevant after APE
    pub(crate) _replayed_slot: PodSlot,

    /// The bank hash of the latest replayed block
    /// Only relevant after APE
    pub(crate) _replayed_bank_hash: Hash,

    /// Associated BLS public key
    pub(crate) bls_pubkey: BlsPubkey,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default, PartialEq)]
/// The most recent timestamp submitted with a notarization vote
pub struct BlockTimestamp {
    pub(crate) slot: PodSlot,
    pub(crate) timestamp: PodUnixTimestamp,
}

impl BlockTimestamp {
    /// The slot that was voted on
    pub fn slot(&self) -> Slot {
        Slot::from(self.slot)
    }

    /// The timestamp
    pub fn timestamp(&self) -> UnixTimestamp {
        UnixTimestamp::from(self.timestamp)
    }
}

impl From<&BlockTimestamp> for LegacyBlockTimestamp {
    fn from(ts: &BlockTimestamp) -> Self {
        LegacyBlockTimestamp {
            slot: ts.slot(),
            timestamp: ts.timestamp(),
        }
    }
}

impl VoteState {
    const VOTE_STATE_VERSION: u8 = 1;

    /// Create a new vote state for tests
    pub fn new_for_tests(
        node_pubkey: Pubkey,
        authorized_voter: Pubkey,
        epoch: Epoch,
        authorized_withdrawer: Pubkey,
        commission: u8,
        bls_pubkey: BlsPubkey,
    ) -> Self {
        Self {
            version: Self::VOTE_STATE_VERSION,
            node_pubkey,
            authorized_voter: AuthorizedVoter {
                epoch: PodU64::from(epoch),
                voter: authorized_voter,
            },
            authorized_withdrawer,
            commission,
            bls_pubkey,
            ..VoteState::default()
        }
    }

    /// Create a new vote state and wrap it in an account
    pub fn create_account_with_authorized(
        node_pubkey: &Pubkey,
        authorized_voter: &Pubkey,
        authorized_withdrawer: &Pubkey,
        commission: u8,
        lamports: u64,
        bls_pubkey: BlsPubkey,
    ) -> AccountSharedData {
        let mut account = AccountSharedData::new(lamports, Self::size(), &crate::id());
        let vote_state = Self::new_for_tests(
            *node_pubkey,
            *authorized_voter,
            0, // Epoch
            *authorized_withdrawer,
            commission,
            bls_pubkey,
        );
        vote_state.serialize_into(account.data_as_mut_slice());
        account
    }

    /// Return whether the vote account is initialized
    pub fn is_initialized(&self) -> bool {
        self.version > 0
    }

    /// Deserialize a vote state from input data.
    /// Callers can use this with the `data` field from an `AccountInfo`
    pub fn deserialize(vote_account_data: &[u8]) -> Result<&VoteState, ProgramError> {
        spl_pod::bytemuck::pod_from_bytes::<VoteState>(vote_account_data)
    }

    /// Serializes a vote state into an output buffer
    /// Callers can use this with the mutable reference to `data` from
    /// an `AccountInfo`
    pub fn serialize_into(&self, vote_account_data: &mut [u8]) {
        vote_account_data.copy_from_slice(bytemuck::bytes_of(self))
    }

    /// The size of the vote account that stores this VoteState
    pub const fn size() -> usize {
        std::mem::size_of::<VoteState>()
    }

    /// Vote state version
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Validator that votes in this account
    pub fn node_pubkey(&self) -> &Pubkey {
        &self.node_pubkey
    }

    /// Signer for withdrawals
    pub fn authorized_withdrawer(&self) -> &Pubkey {
        &self.authorized_withdrawer
    }

    /// Percentage (0-100) that represents what part of a rewards
    /// payout should be given to this VoteAccount
    pub fn commission(&self) -> u8 {
        self.commission
    }

    /// The authorized voter for the given epoch
    pub fn get_authorized_voter(&self, epoch: Epoch) -> Option<Pubkey> {
        if let Some(av) = self.next_authorized_voter {
            if epoch >= av.epoch() {
                return Some(av.voter);
            }
        }
        if epoch >= self.authorized_voter.epoch() {
            return Some(self.authorized_voter.voter);
        }
        None
    }

    /// Get rent exempt reserve
    pub fn get_rent_exempt_reserve(rent: &Rent) -> u64 {
        rent.minimum_balance(Self::size())
    }

    /// The signer for vote transactions in this epoch
    pub fn authorized_voter(&self) -> &AuthorizedVoter {
        &self.authorized_voter
    }

    /// The signer for vote transactions in an upcoming epoch
    pub fn next_authorized_voter(&self) -> Option<&AuthorizedVoter> {
        self.next_authorized_voter.as_ref()
    }

    /// How many credits this validator is earning in this Epoch
    pub fn epoch_credits(&self) -> &EpochCredit {
        &self.epoch_credits
    }

    /// Most recent timestamp submitted with a vote
    pub fn latest_timestamp_legacy_format(&self) -> LegacyBlockTimestamp {
        // TODO: fix once we figure out how to do timestamps in BLS
        LegacyBlockTimestamp::from(&BlockTimestamp::default())
    }

    /// Set the node_pubkey
    pub fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        self.node_pubkey = node_pubkey
    }

    /// Set the authorized withdrawer
    pub fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        self.authorized_withdrawer = authorized_withdrawer
    }

    /// Set the commission
    pub fn set_commission(&mut self, commission: u8) {
        self.commission = commission
    }

    /// Set the authorized voter
    pub fn set_authorized_voter(&mut self, authorized_voter: AuthorizedVoter) {
        self.authorized_voter = authorized_voter
    }

    /// Set the next authorized voter
    pub fn set_next_authorized_voter(&mut self, next_authorized_voter: AuthorizedVoter) {
        self.next_authorized_voter = Some(next_authorized_voter)
    }

    /// Set the epoch credits
    pub fn set_epoch_credits(&mut self, epoch_credits: EpochCredit) {
        self.epoch_credits = epoch_credits
    }

    /// Get the BLS pubkey
    pub fn bls_pubkey(&self) -> &BlsPubkey {
        &self.bls_pubkey
    }
}
