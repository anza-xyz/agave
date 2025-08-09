//! Message types for Votor

#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

pub mod bls_message;
pub mod state;
pub mod vote;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
