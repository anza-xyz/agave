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
pub mod send_transaction_service;
pub mod send_transaction_service_stats;
#[cfg(any(test, feature = "dev-context-only-utils"))]
pub mod test_utils;
pub mod tpu_info;
pub mod transaction_client;

pub use {
    send_transaction_service_stats::SendTransactionServiceStats,
    transaction_client::{CurrentLeaderInfo, LEADER_INFO_REFRESH_RATE_MS},
};

#[macro_use]
extern crate solana_metrics;
