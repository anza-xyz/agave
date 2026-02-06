#![cfg(feature = "agave-unstable-api")]
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
pub mod consensus_metrics;
mod consensus_pool;
mod consensus_pool_service;
pub mod event;
mod event_handler;
pub mod root_utils;
mod staked_validators_cache;
mod timer_manager;
pub mod vote_history;
pub mod vote_history_storage;
pub mod voting_service;
pub mod voting_utils;
pub mod votor;
mod welford_stats;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
