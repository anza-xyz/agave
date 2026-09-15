#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

use {solana_clock::Slot, solana_pubkey::Pubkey};

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
/// A message is a slot and a list of validators who sent a valid vote for that slot.
pub type VerifiedVotorSlotsMessage = (Slot, Vec<Pubkey>);
