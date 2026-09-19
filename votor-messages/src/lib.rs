#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

use {
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

pub mod certificate;
pub mod consensus_message;
pub mod finalized_slot;
pub mod fraction;
pub mod metric_types;
pub mod migration;
pub mod reward_certificate;
pub mod sig_verified_messages;
pub mod unverified_vote_message;
pub mod vote;
pub mod wire;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

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
/// A message is a HashMap mapping slots to the list of validators from whom a valid vote in that
/// slot was received.
pub type VerifiedVotorSlotsMessage = HashMap<Slot, VoteAccountPubkeys>;
