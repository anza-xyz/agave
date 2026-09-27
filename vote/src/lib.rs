#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

pub mod vote_account;
pub mod vote_parser;
pub mod vote_state_view;
pub mod vote_state_view_mut;
pub mod vote_transaction;

#[cfg_attr(feature = "stable-abi", macro_use)]
#[cfg(feature = "stable-abi")]
extern crate solana_frozen_abi_macro;
