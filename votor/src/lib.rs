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
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

#[macro_use]
extern crate log;

pub mod commitment;
pub mod common;
mod consensus_metrics;
pub mod consensus_pool;
mod consensus_pool_service;
pub mod event;
mod event_handler;
pub mod root_utils;
mod staked_validators_cache;
mod timer_manager;
pub mod vote_history;
pub mod vote_history_storage;
mod voting_service;
mod voting_utils;
#[allow(dead_code)]
pub mod votor;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
