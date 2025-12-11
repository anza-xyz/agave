#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

pub mod commitment;
pub mod common;
pub mod consensus_metrics;
pub mod consensus_pool;
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

#[macro_use]
extern crate log;

extern crate serde_derive;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
