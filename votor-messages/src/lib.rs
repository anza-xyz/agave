#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

pub mod consensus_message;
pub mod migration;
pub mod reward_certificate;
pub mod vote;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
