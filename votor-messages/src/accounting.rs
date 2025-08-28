//! Accounting related operations on the Vote Account
use {
    crate::state::PodEpoch,
    bytemuck::{Pod, PodInOption, Zeroable, ZeroableInOption},
    solana_program::{clock::Epoch, pubkey::Pubkey},
    spl_pod::primitives::PodU64,
};

/// Authorized Signer for vote instructions
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default, PartialEq)]
pub struct AuthorizedVoter {
    pub(crate) epoch: PodEpoch,
    pub(crate) voter: Pubkey,
}

impl AuthorizedVoter {
    /// Create a new authorized voter for `epoch`
    pub fn new(epoch: Epoch, voter: Pubkey) -> Self {
        Self {
            epoch: PodEpoch::from(epoch),
            voter,
        }
    }

    /// Get the authorization epoch
    pub fn epoch(&self) -> Epoch {
        Epoch::from(self.epoch)
    }

    /// Get the voter that is authorized
    pub fn voter(&self) -> &Pubkey {
        &self.voter
    }

    /// Set the authorization epoch
    pub fn set_epoch(&mut self, epoch: Epoch) {
        self.epoch = PodEpoch::from(epoch);
    }

    /// Set the voter that is authorized
    pub fn set_voter(&mut self, voter: Pubkey) {
        self.voter = voter;
    }
}

// UNSAFE: we require that `epoch > 0` so this is safe
unsafe impl ZeroableInOption for AuthorizedVoter {}
unsafe impl PodInOption for AuthorizedVoter {}

/// The credits information for an epoch
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default, PartialEq)]
pub struct EpochCredit {
    pub(crate) epoch: PodEpoch,
    pub(crate) credits: PodU64,
    pub(crate) prev_credits: PodU64,
}

impl EpochCredit {
    /// Create a new epoch credit
    pub fn new(epoch: Epoch, credits: u64, prev_credits: u64) -> Self {
        Self {
            epoch: PodEpoch::from(epoch),
            credits: PodU64::from(credits),
            prev_credits: PodU64::from(prev_credits),
        }
    }

    /// Get epoch in which credits were earned
    pub fn epoch(&self) -> u64 {
        u64::from(self.epoch)
    }

    /// Get the credits earned
    pub fn credits(&self) -> u64 {
        u64::from(self.credits)
    }

    /// Get the credits earned in the previous epoch
    pub fn prev_credits(&self) -> u64 {
        u64::from(self.prev_credits)
    }

    /// Set epoch in which credits were earned
    pub fn set_epoch(&mut self, epoch: Epoch) {
        self.epoch = PodEpoch::from(epoch);
    }

    /// Set the credits earned
    pub fn set_credits(&mut self, credits: u64) {
        self.credits = PodU64::from(credits);
    }

    /// Set the credits earned in the previous epoch
    pub fn set_prev_credits(&mut self, prev_credits: u64) {
        self.prev_credits = PodU64::from(prev_credits);
    }
}
