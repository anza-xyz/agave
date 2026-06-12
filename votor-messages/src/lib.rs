#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

use {
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
};

pub mod certificate;
pub mod consensus_message;
pub mod fraction;
pub mod metric_types;
pub mod migration;
pub mod reward_certificate;
pub mod vote;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

/// Send side of verified voter channel.
/// Each message contains the Pubkey of the voter and the slots in last verified vote.
pub type VerifiedVoterSlotsSender = Sender<(Pubkey, Vec<Slot>)>;
/// Receive side of verified voter channel.
pub type VerifiedVoterSlotsReceiver = Receiver<(Pubkey, Vec<Slot>)>;
