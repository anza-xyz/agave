#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects)]
mod cluster_tpu_info;
pub mod filter;
pub mod max_slots;
pub mod optimistically_confirmed_bank_tracker;
pub mod parsed_token_accounts;
pub mod rpc;
mod rpc_cache;
pub mod rpc_completed_slots_service;
pub mod rpc_health;
pub mod rpc_pubsub;
pub mod rpc_pubsub_service;
pub mod rpc_service;
pub mod rpc_subscription_tracker;
pub mod rpc_subscriptions;
pub mod slot_status_notifier;
pub mod transaction_notifier_interface;
pub mod transaction_status_service;

#[macro_use]
extern crate log;

#[cfg(test)]
#[macro_use]
extern crate serde_json;

#[macro_use]
extern crate solana_metrics;
