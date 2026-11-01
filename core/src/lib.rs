#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]
#![recursion_limit = "2048"]
//! The `solana` library implements the Solana high-performance blockchain architecture.
//! It includes a full Rust implementation of the architecture (see
//! [Validator](validator/struct.Validator.html))).  It also includes
//! command-line tools to spin up validators and a Rust library
//!

pub mod admin_rpc_post_init;
pub mod banking_channels;
pub mod banking_stage;
pub(crate) mod block_creation_loop;
pub mod commitment_service;
pub mod completed_data_sets_service;
pub mod consensus;
pub mod cost_update_service;
pub mod drop_bank_service;
pub mod epoch_specs;
pub mod fetch_stage;
pub mod forwarding_stage;
pub mod gen_keys;
pub mod repair;
pub mod replay_stage;
pub mod resource_limits;
mod result;
pub mod sample_performance_service;
#[cfg(unix)]
mod scheduler_bindings_server;
mod shred_fetch_stage;
pub mod sigverify;
pub mod sigverify_stage;
pub mod snapshot_packager_service;
pub mod staked_nodes_updater_service;
pub mod stats_reporter_service;
pub mod system_monitor_service;
pub mod tpu;
mod tpu_entry_notifier;
mod transaction_priority;
pub mod tvu;
pub mod validator;
pub mod window_service;

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;
