#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects)]
pub mod system_instruction;
pub mod system_processor;

use solana_sdk_ids::system_program;
pub use {
    solana_nonce_account::{get_system_account_kind, SystemAccountKind},
    system_program::id,
};
