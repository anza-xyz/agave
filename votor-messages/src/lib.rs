#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

use {
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
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

/// Message type for the verified voter channel.
/// A message is a HashMap mapping slots to the list of validators from whom a valid vote in that
/// slot was received.
type VerifiedVotorSlotsMessage = HashMap<Slot, Vec<Pubkey>>;
/// Send side of verified voter channel.
pub type VerifiedVoterSlotsSender = Sender<VerifiedVotorSlotsMessage>;
/// Receive side of verified voter channel.
pub type VerifiedVoterSlotsReceiver = Receiver<VerifiedVotorSlotsMessage>;
