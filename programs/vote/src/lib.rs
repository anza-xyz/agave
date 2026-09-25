#![cfg(feature = "agave-unstable-api")]

pub mod vote_processor;
pub mod vote_state;

#[cfg(feature = "stable-abi")]
extern crate solana_frozen_abi_macro;

pub use solana_vote_interface::{
    authorized_voters, error as vote_error, instruction as vote_instruction,
    program::{check_id, id},
};
