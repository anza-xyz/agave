#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

pub mod vote_processor;
pub mod vote_state;

#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

pub use solana_vote_interface::{
    authorized_voters, error as vote_error, instruction as vote_instruction,
    program::{check_id, id},
};
