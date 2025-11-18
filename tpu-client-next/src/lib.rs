#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
//! # Feature flags
//!
//! Tpu-client-next supports three features:
//!
//! - **`metrics`**: Enables implementation of the method `report_to_influxdb` for
//!   [`SendTransactionStats`] structure.
//! - **`log`**: Enables logging using `log` crate. It is enabled by default.
//! - **`tracing`**: Enables logging using `tracing` crate instead of `log`. This feature is
//!   mutually exclusive with `log`.
//! - **`websocket-node-address-service`**: Enables implementation of
//!   `WebsocketNodeAddressService` that provides slot updates via WebSocket interface.

pub mod client_builder;
pub(crate) mod connection_worker;
pub mod connection_workers_scheduler;
pub mod send_transaction_stats;
pub mod workers_cache;
pub use crate::{
    client_builder::{Client, ClientBuilder, ClientError, TransactionSender},
    connection_workers_scheduler::{ConnectionWorkersScheduler, ConnectionWorkersSchedulerError},
    send_transaction_stats::SendTransactionStats,
};
pub(crate) mod quic_networking;
pub(crate) use crate::quic_networking::QuicError;
pub mod leader_updater;
pub mod transaction_batch;

#[cfg(feature = "metrics")]
pub mod metrics;

// Logging abstraction module
pub(crate) mod logging;

pub mod node_address_service;
#[cfg(feature = "websocket-node-address-service")]
pub mod websocket_node_address_service;
