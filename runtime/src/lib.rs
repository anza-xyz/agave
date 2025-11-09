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
#![allow(clippy::arithmetic_side_effects)]

mod account_saver;
pub mod accounts_background_service;
pub mod bank;
pub mod bank_client;
pub mod bank_forks;
pub mod bank_hash_cache;
pub mod bank_utils;
pub mod commitment;
pub mod dependency_tracker;
pub mod epoch_stakes;
pub mod genesis_utils;
pub mod inflation_rewards;
pub mod installed_scheduler_pool;
pub mod loader_utils;
pub mod non_circulating_supply;
pub mod prioritization_fee;
pub mod prioritization_fee_cache;
mod read_optimized_dashmap;
pub mod rent_collector;
pub mod runtime_config;
pub mod serde_snapshot;
pub mod snapshot_bank_utils;
pub mod snapshot_controller;
pub mod snapshot_minimizer;
pub mod snapshot_package;
pub mod snapshot_utils;
mod stake_account;
pub mod stake_history;
pub mod stake_utils;
pub mod stake_weighted_timestamp;
pub mod stakes;
pub mod static_ids;
pub mod status_cache;
pub mod transaction_batch;
pub mod vote_sender_types;

#[macro_use]
extern crate solana_metrics;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
