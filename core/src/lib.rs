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
#![recursion_limit = "2048"]
//! The `solana` library implements the Solana high-performance blockchain architecture.
//! It includes a full Rust implementation of the architecture (see
//! [Validator](validator/struct.Validator.html))).  It also includes
//! command-line tools to spin up validators and a Rust library
//!

pub mod admin_rpc_post_init;
pub mod banking_simulation;
pub mod banking_stage;
pub mod banking_trace;
#[allow(dead_code)]
mod block_creation_loop;
pub mod cluster_info_vote_listener;
pub mod cluster_slots_service;
pub mod commitment_service;
pub mod completed_data_sets_service;
pub mod consensus;
pub mod cost_update_service;
pub mod drop_bank_service;
pub mod fetch_stage;
pub mod forwarding_stage;
pub mod gen_keys;
mod mock_alpenglow_consensus;
pub mod next_leader;
pub mod optimistic_confirmation_verifier;
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
pub mod tvu;
pub mod unfrozen_gossip_verified_vote_hashes;
pub mod validator;
mod vortexor_receiver_adapter;
pub mod vote_simulator;
pub mod voting_service;
pub mod warm_quic_cache_service;
pub mod window_service;

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;
