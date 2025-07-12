#![allow(clippy::arithmetic_side_effects)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(missing_unsafe_on_extern)]
#![warn(rust_2024_guarded_string_incompatible_syntax)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

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

#[macro_use]
extern crate serde_derive;

#[cfg(test)]
#[macro_use]
extern crate serde_json;

#[macro_use]
extern crate solana_metrics;
