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
pub mod poh_controller;
pub mod poh_recorder;
pub mod poh_service;
pub mod record_channels;
pub mod transaction_recorder;

#[macro_use]
extern crate solana_metrics;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;
