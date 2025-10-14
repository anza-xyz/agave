#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[macro_use]
extern crate log;

pub mod commitment;
pub mod common;
mod consensus_metrics;
pub mod consensus_pool;
#[allow(dead_code)]
mod consensus_pool_service;
pub mod event;
#[allow(dead_code)]
mod event_handler;
pub mod root_utils;
mod staked_validators_cache;
mod timer_manager;
pub mod vote_history;
pub mod vote_history_storage;
mod voting_service;
mod voting_utils;
#[allow(dead_code)]
mod votor;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
