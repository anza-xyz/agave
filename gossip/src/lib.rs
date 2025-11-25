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
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

pub mod cluster_info;
pub mod cluster_info_metrics;
pub mod contact_info;
pub mod crds;
pub mod crds_data;
pub mod crds_entry;
mod crds_filter;
pub mod crds_gossip;
pub mod crds_gossip_error;
pub mod crds_gossip_pull;
pub mod crds_gossip_push;
pub mod crds_shards;
pub mod crds_value;
mod deprecated;
pub mod duplicate_shred;
pub mod duplicate_shred_handler;
pub mod duplicate_shred_listener;
pub mod epoch_slots;
pub mod epoch_specs;
pub mod gossip_error;
pub mod gossip_service;
pub mod node;
#[macro_use]
mod tlv;
#[macro_use]
mod legacy_contact_info;
pub mod ping_pong;
mod protocol;
mod push_active_set;
mod received_cache;
pub mod restart_crds_values;
pub mod weighted_shuffle;

#[macro_use]
extern crate log;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

#[macro_use]
extern crate solana_metrics;

mod wire_format_tests;
