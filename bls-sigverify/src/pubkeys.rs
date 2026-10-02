use {solana_clock::Slot, solana_pubkey::Pubkey, std::sync::Arc};

#[derive(Debug, PartialEq, Eq)]
/// Different ways of storing a list of vote account pubkeys.
pub enum VoteAccountPubkeys {
    /// A shared list of pubkeys.
    Shared(Arc<Vec<Pubkey>>),
    /// an owned list of pubkeys.
    Owned(Vec<Pubkey>),
}

impl VoteAccountPubkeys {
    /// Returns a reference to the list of pubkeys.
    pub fn as_slice(&self) -> &[Pubkey] {
        match self {
            Self::Shared(p) => p,
            Self::Owned(p) => p,
        }
    }
}

/// Message type for the verified voter channel.
/// A message is a slot and a list of validators who sent a valid vote for that slot.
pub type VerifiedVotorSlotsMessage = (Slot, VoteAccountPubkeys);
