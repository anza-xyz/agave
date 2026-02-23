#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]
pub mod system_instruction;
pub mod system_processor;

use solana_sdk_ids::system_program;
pub use {
    solana_nonce_account::{SystemAccountKind, get_system_account_kind},
    system_program::id,
};
