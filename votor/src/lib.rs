#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[cfg(feature = "agave-unstable-api")]
pub mod commitment;

#[cfg(feature = "agave-unstable-api")]
pub mod common;

#[cfg(feature = "agave-unstable-api")]
pub mod consensus_pool;

#[cfg(feature = "agave-unstable-api")]
pub mod event;

#[cfg(feature = "agave-unstable-api")]
pub mod root_utils;

#[cfg(feature = "agave-unstable-api")]
#[macro_use]
extern crate log;

#[cfg(feature = "agave-unstable-api")]
extern crate serde_derive;

#[cfg(feature = "agave-unstable-api")]
mod staked_validators_cache;

#[cfg(feature = "agave-unstable-api")]
mod timer_manager;

#[cfg(feature = "agave-unstable-api")]
pub mod vote_history;
#[cfg(feature = "agave-unstable-api")]
pub mod vote_history_storage;

#[cfg(feature = "agave-unstable-api")]
mod voting_service;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
