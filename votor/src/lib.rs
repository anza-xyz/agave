#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[macro_use]
extern crate log;
extern crate serde_derive;

#[cfg(feature = "agave-unstable-api")]
pub mod commitment;

#[cfg(feature = "agave-unstable-api")]
pub mod common;

mod consensus_metrics;
mod consensus_pool;
mod consensus_pool_service;

#[cfg(feature = "agave-unstable-api")]
pub mod event;

mod event_handler;

#[cfg(feature = "agave-unstable-api")]
pub mod root_utils;

mod staked_validators_cache;
mod timer_manager;

#[cfg(feature = "agave-unstable-api")]
pub mod vote_history;
#[cfg(feature = "agave-unstable-api")]
pub mod vote_history_storage;

mod voting_service;
mod voting_utils;

#[cfg(feature = "agave-unstable-api")]
pub mod votor;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
