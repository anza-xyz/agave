//! Alpenglow Vote program
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

pub mod accounting;
pub mod consensus_message;
pub mod instruction;
pub mod state;
pub mod vote;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

// Export current SDK types for downstream users building with a different SDK
// version
pub use solana_program;

solana_program::declare_id!("Vote222222222222222222222222222222222222222");
