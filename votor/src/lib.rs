#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

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
